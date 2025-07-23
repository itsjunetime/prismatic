use core::{error::Error, fmt::Display};
use std::{
	collections::{BTreeMap, BTreeSet},
	fs::read_link,
	io::ErrorKind,
	path::Path,
	sync::{
		PoisonError,
		mpsc::{self, Receiver, channel}
	}
};

use eframe::NativeOptions;
use egui::{Align, Label, Layout, UiBuilder, Vec2, Vec2b, vec2};
use tokio::runtime::Runtime;

use crate::{
	config::{FileConfig, SmapiConfig},
	dirs::{Dirs, get_modgroups_from_data_dir},
	fetch::{BrowserMessage, launch_browser},
	mod_group::{
		Mod, ModGroup, ModGroupCreationErr, UniqueId, collect_mods_in_path, delete_mod,
		make_files_for_modgroup
	},
	runner::RunningInstance
};

mod config;
mod dirs;
mod fetch;
mod mod_group;
mod runner;

fn main() -> Result<(), Box<dyn Error>> {
	let mut logger = None;
	if std::env::var("RUST_LOG").is_ok() {
		logger = Some(flexi_logger::Logger::try_with_env()?.start()?);
	}

	let native_options = NativeOptions::default();
	eframe::run_native(
		"Prismatic",
		native_options,
		Box::new(|cc| {
			catppuccin_egui::set_theme(&cc.egui_ctx, catppuccin_egui::FRAPPE);
			Ok(Box::new(App::default()))
		})
	)?;

	drop(logger);
	Ok(())
}

const APP_NAME: &str = "prismatic";

struct App {
	runtime: Runtime,
	dirs: crate::dirs::Dirs,
	all_mods: AllMods,
	modgroups: BTreeSet<ModGroup>,
	visible_errors: Vec<String>,
	receiver: Receiver<AppMsg>,
	current_run: Option<RunningDisplay>,
	modal: Option<DisplayedModal>,
	discovering_new_mods_from: Option<Receiver<AppMsg>>,
	config: FileConfig,
	browser: Option<BrowserSession>
}

enum DisplayedModal {
	NewModGroup(NewModGroup),
	DeletingMod {
		to_delete: UniqueId,
		also_delete: BTreeSet<UniqueId>
	}
}

// a map of a unique id to every single mod that depends on it. We don't track whether it is
// depended on in a 'required' or 'optional' way because we only access it with mods that we
// currently have on-system.
pub type DependentsMap = BTreeMap<UniqueId, BTreeSet<UniqueId>>;

#[derive(Default)]
struct AllMods {
	mods: BTreeMap<UniqueId, Mod>,
	dependents: DependentsMap
}

impl AllMods {
	pub fn insert_mod(&mut self, modd: Mod) {
		let all_deps = modd
			.dependencies
			.iter()
			.map(|d| &d.unique_id.0)
			.chain(modd.content_pack_for.as_ref().map(|p| &p.unique_id));

		for dependency_id in all_deps {
			match self.dependents.get_mut(dependency_id) {
				Some(dependents) => _ = dependents.insert(modd.unique_id.clone()),
				None =>
					_ = self.dependents.insert(
						UniqueId(dependency_id.clone()),
						BTreeSet::from_iter([modd.unique_id.clone()])
					),
			}
		}

		self.mods.insert(modd.unique_id.clone(), modd);
	}
}

#[derive(Default)]
struct NewModGroup {
	wip: ModGroup,
	dependencies_expanded: BTreeSet<UniqueId>,
	dependents: DependentsMap
}

struct RunningDisplay {
	logs_displayed: bool,
	instance: RunningInstance
}

pub struct BrowserSession {
	task: tokio::task::JoinHandle<()>,
	receiver: Receiver<BrowserMessage>
}

// It's fine to have large variants here 'cause these should only ever be present within a
// `Sender`/`Receiver` pair, meaning that they'd already be on the heap. So Boxing the biggest
// variant would allow the allocation that this resides in to be smaller, but would make us do 2
// allocations instead of just 1 for the Box'd variant. So in most cases, I'm in favor of boxing
// the largest variant, but here I don't think it's really worth it.
#[expect(clippy::large_enum_variant)]
enum AppMsg {
	ModGroupDiscovered(mod_group::ModGroup),
	ModDiscovered(mod_group::Mod),
	UserRelevantError(String)
}

impl Default for App {
	fn default() -> Self {
		let mut visible_errors = Vec::default();

		let config = ::dirs::config_local_dir()
			.and_then(|path| {
				FileConfig::from_path(&path.join("prismatic.kdl"))
					.inspect_err(|e| visible_errors.push(format!("Can't load config file: {e}")))
					.ok()
			})
			.unwrap_or_default();

		let dirs = crate::dirs::Dirs::default();

		for dir in [&dirs.modgroups_dir, &dirs.mod_dir] {
			match std::fs::exists(dir) {
				Ok(true) => continue,
				Ok(false) => (),
				Err(e) => visible_errors.push(format!(
					"Couldn't check if required dir {dir:?} exists: {e}"
				))
			};

			if let Err(e) = std::fs::create_dir_all(dir) {
				visible_errors.push(format!("Couldn't create necessary dir {dir:?}: {e}"));
			}
		}

		let runtime = tokio::runtime::Runtime::new().expect(
			"We couldn't initialize the tokio runtime. Nothing can work if we can't do this."
		);

		let (sender, receiver) = mpsc::channel();

		let modgroups_sender = sender.clone();
		let modgroups_dir = dirs.modgroups_dir.clone();

		runtime.spawn(async move {
			get_modgroups_from_data_dir(&modgroups_dir, &modgroups_sender).await
		});

		let mods_sender = sender.clone();
		let mods_dir = dirs.mod_dir.clone();
		let mut id_accum = BTreeSet::default();
		runtime.spawn(
			async move { collect_mods_in_path(&mods_dir, &mods_sender, &mut id_accum).await }
		);

		Self {
			runtime,
			dirs,
			// this'll be populated as the rayon background stuff runs
			all_mods: AllMods::default(),
			// this'll also be populated as the rayon stuff runs
			modgroups: BTreeSet::default(),
			visible_errors,
			receiver,
			current_run: None,
			modal: None,
			discovering_new_mods_from: None,
			config,
			browser: None
		}
	}
}

impl eframe::App for App {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		while let Ok(msg) = self.receiver.try_recv() {
			self.handle_msg(msg);
		}

		ctx.set_zoom_factor(self.config.zoom_factor);

		self.check_if_discovered_new_mod();
		self.new_modgroup_view(ctx);

		egui::CentralPanel::default().show(ctx, |ui| {
			ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
				let mut remove_idx = None;

				for (idx, error) in self.visible_errors.iter().enumerate() {
					ui.with_layout(Layout::left_to_right(Align::BOTTOM), |ui| {
						if ui.button("❌").clicked() {
							remove_idx = Some(idx);
						}

						ui.add(Label::new(error).wrap())
					});
				}

				if let Some(idx) = remove_idx {
					self.visible_errors.remove(idx);
				}

				ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
					ui.horizontal(|ui| {
						ui.heading("Prismatic");

						if let Some(RunningDisplay {
							ref mut logs_displayed,
							instance: _
						}) = self.current_run && !*logs_displayed
							&& ui.button("View Running Logs").clicked()
						{
							*logs_displayed = true;
						}

						ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
							/*ui.add_enabled_ui(self.browser.is_none(), |ui| {
								if ui.button("Log In!").clicked() {
									self.spawn_browser();
								}
							});*/

							ui.add_enabled_ui(self.modal.is_none(), |ui| {
								if ui.button("+ New ModGroup").clicked() {
									self.modal =
										Some(DisplayedModal::NewModGroup(NewModGroup::default()));
								}
							});

							if ui.button("+ Mod (from computer)").clicked() {
								self.discovering_new_mods_from =
									start_discovering_new_mods_on_fs(&self.runtime);
							}
						})
					});

					let mut new_run = None;
					match &mut self.current_run {
						Some(RunningDisplay {
							logs_displayed,
							instance
						}) if *logs_displayed => Self::logs_view(
							ui,
							logs_displayed,
							instance,
							&mut new_run,
							&self.config.smapi_config,
							&self.dirs,
							&mut self.visible_errors
						),
						_ => self.mods_and_modgroup_area(ui)
					}

					if let Some(instance) = new_run {
						self.current_run = Some(RunningDisplay {
							instance,
							logs_displayed: true
						});
					}
				});
			});
		});
	}
}

impl App {
	fn mods_and_modgroup_area(&mut self, ui: &mut egui::Ui) {
		split_horiz(
			ui,
			ui.available_height(),
			|ui| {
				Self::modgroup_area(
					ui,
					&self.modgroups,
					&mut self.current_run,
					&mut self.visible_errors,
					&self.dirs,
					&self.config.smapi_config
				)
			},
			|ui| {
				ui.heading("Mods");

				egui::ScrollArea::vertical().show(ui, |ui| {
					for m in self.all_mods.mods.values() {
						ui.horizontal(|ui| {
							ui.label(m.user_visible_name(&self.all_mods.mods));

							ui.with_layout(Layout::left_to_right(Align::BOTTOM), |ui| {
								let dependents = self.all_mods.dependents.get(&m.unique_id);
								let is_enabled = dependents.is_none_or(BTreeSet::is_empty);
								let mut resp = ui
									.add_enabled_ui(is_enabled, |ui| {
										if ui.button("🗑").clicked() {
											self.modal = Some(DisplayedModal::DeletingMod {
												to_delete: m.unique_id.clone(),
												also_delete: BTreeSet::default()
											});
										}
									})
									.response;

								if let Some(deps) = dependents
									&& !deps.is_empty()
								{
									let hover_text = format!(
										"{} is required by {}",
										m.name,
										deps.iter()
											.filter_map(|d| self.all_mods.mods.get(d))
											.map(|m| m.user_visible_name(&self.all_mods.mods))
											.collect::<Vec<_>>()
											.join(", ")
									);
									resp = resp.on_disabled_hover_text(hover_text);
								}

								resp
							});
						});
					}
				});
			}
		);
	}

	fn modgroup_area(
		ui: &mut egui::Ui,
		modgroups: &BTreeSet<ModGroup>,
		current_run: &mut Option<RunningDisplay>,
		visible_errors: &mut Vec<String>,
		dirs: &Dirs,
		config: &SmapiConfig
	) {
		ui.heading("Mod Groups");
		for group in modgroups {
			let this_is_running = current_run
				.as_ref()
				.is_some_and(|r| r.instance.name == group.name);

			ui.horizontal(|ui| {
				if ui.button(&group.name).clicked() {
					if let Some(RunningDisplay {
						logs_displayed: _,
						instance
					}) = current_run.as_mut()
					{
						match instance.child.try_wait() {
							Err(e) => visible_errors.push(format!(
								"Couldn't check if currently-running instace of stardew (with pid {}) is actually still running: {e}. You should probably restart this app.",
								instance.child.id()
							)),
							Ok(None) => (), // It's still running, whatever.
							// TODO: Show this status somehow?
							//
							// If it already exited, then just hide it. Nice and easy.
							Ok(Some(_)) => *current_run = None,
						}
					}

					match &current_run {
						None => match RunningInstance::try_new(
							&group.name,
							config,
							// TODO: Allow selecting
							dirs.stardew_paths.iter().next().unwrap(),
							&dirs.modgroups_dir
						) {
							Ok(instance) => {
								println!("got child with pid {}", instance.child.id());
								*current_run = Some(RunningDisplay {
									logs_displayed: true,
									instance
								});
							}
							Err(e) => panic!("{e}") // TODO: Handle
						},
						Some(RunningDisplay {
							logs_displayed: _,
							instance
						}) => {
							let modal = egui_modal::Modal::new(ui.ctx(), "try_run_failed");
							modal.show(|ui| {
								modal.title(ui, "Stardew Already Running");

								modal.frame(ui, |ui| {
									modal.body(
										ui,
										format!(
											"Stardew is already running under modgroup {}, and we can't run it twice at the same time. \n\nPlease close the currently running instance and try again",
											instance.name
										)
									);
								});

								// TODO: Allow user to click a button to kill it and then send the
								// child to a separate thread to kill and show updates in the UI
								modal.suggested_button(ui, "OK");
							});

							modal.open();
						}
					}
				}

				if this_is_running {
					ui.strong("(Running)");
				}
			});
		}
	}

	fn handle_msg(&mut self, msg: AppMsg) {
		Self::handle_msg_inner(
			msg,
			&mut self.visible_errors,
			&mut self.all_mods,
			&mut self.modgroups,
			&self.dirs
		);
	}

	fn handle_msg_inner(
		msg: AppMsg,
		visible_errors: &mut Vec<String>,
		all_mods: &mut AllMods,
		modgroups: &mut BTreeSet<ModGroup>,
		dirs: &dirs::Dirs
	) {
		match msg {
			AppMsg::UserRelevantError(s) => visible_errors.push(s),
			AppMsg::ModDiscovered(m) => {
				// We don't care if there was already one in there - if modgroups have
				// overlapping mods at all, we will be inserting the same mod multiple times,
				// and that's fine. I don't think there's really any easy way to deduplicate
				// work if we want to be fault-tolerant to someone messing with the directories
				// we're storing these in.
				all_mods.insert_mod(m);
			}
			AppMsg::ModGroupDiscovered(mod_group) => {
				let name = mod_group.name.clone();
				if !modgroups.insert(mod_group) {
					visible_errors.push(format!(
						"Two mod groups with the name {name:?} detected; we can't handle multiple mod groups with the same name, so go clean up your modgroup directory (at {:?}) so no two top-level folders have the same name",
						dirs.modgroups_dir
					));
				}
			}
		};
	}

	fn check_if_discovered_new_mod(&mut self) {
		if let Some(ref new_mod_recv) = self.discovering_new_mods_from {
			while let Ok(mut msg) = new_mod_recv.try_recv() {
				if let AppMsg::ModDiscovered(ref mut new_mod) = msg {
					let Some(parent_dir) = new_mod.manifest_path.parent() else {
						self.visible_errors.push(format!(
							"Can't process mod '{}' due to a malformed detected path ({:?}). This is a bug in Primsatic; please report it.",
							new_mod.name,
							new_mod.manifest_path
						));
						return;
					};

					let new_mod_dir = self.dirs.mod_dir.join(&new_mod.unique_id.0);
					if let Err(e) = std::fs::create_dir(&new_mod_dir) {
						self.visible_errors.push(format!(
							"Couldn't make new directory for mod '{}' at {:?}: {e}",
							new_mod.name, new_mod_dir
						));
						return;
					}

					if let Err((err, io_err)) =
						copy_contents_of_dir_into_new_dir(parent_dir, &new_mod_dir)
					{
						self.visible_errors.push(format!(
							"Couldn't create new directory for mod: {err}: {io_err}"
						));
					}
				}

				Self::handle_msg_inner(
					msg,
					&mut self.visible_errors,
					&mut self.all_mods,
					&mut self.modgroups,
					&self.dirs
				);
			}
		}
	}

	fn logs_view(
		ui: &mut egui::Ui,
		displayed: &mut bool,
		instance: &mut RunningInstance,
		new_run: &mut Option<RunningInstance>,
		smapi_config: &SmapiConfig,
		dirs: &Dirs,
		visible_errors: &mut Vec<String>
	) {
		ui.horizontal(|ui| {
			ui.heading(format!("Currently running ModGroup '{}'", instance.name));
			if ui.button("Hide Logs").clicked() {
				*displayed = false;
			}
		});

		ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
			ui.horizontal(|ui| {
				if ui.button("Restart").clicked() {
					if let Err(e) = instance.child.kill() {
						visible_errors
							.push(format!("Couldn't kill currently-running stardew: {e}"));
					};
					*new_run = match RunningInstance::try_new(
						&instance.name,
						smapi_config,
						dirs.stardew_paths.first().unwrap(),
						&dirs.modgroups_dir
					) {
						Ok(instance) => Some(instance),
						Err(e) => {
							visible_errors
								.push(format!("Couldn't start stardew back up again: {e}"));
							None
						}
					};
				} else if ui.button("Stop Game").clicked()
					&& let Err(e) = instance.child.kill()
				{
					visible_errors.push(format!("Couldn't kill stardew: {e}"));
				}
			});

			ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
				egui::ScrollArea::vertical()
					.id_salt("logs_buf")
					.show(ui, |ui| {
						let borrowed_vec = instance
							.logs_buf
							.lock()
							.unwrap_or_else(PoisonError::into_inner);

						match str::from_utf8(&borrowed_vec) {
							Ok(text) => ui.label(text),
							Err(e) => ui.label(format!(
								"SMAPI output contains non-unicode characters, so we can't display it: {e}"
							))
						};

						match instance.child.try_wait() {
							Err(e) =>
								_ = ui.label(format!(
									"We can't determine if the process has exited or not: {e}"
								)),
							Ok(Some(code)) =>
								_ = ui.label(format!("Process exited with code {code}")),
							Ok(None) => () // it's still running, continue
						}
					});
			});
		});
	}

	fn new_modgroup_view(&mut self, ctx: &egui::Context) {
		let Some(displayed) = &mut self.modal else {
			return;
		};

		let modal = egui_modal::Modal::new(ctx, "modal");
		let mut create_button = None;
		let mut cancel_button = None;

		// let window_height = ctx.available_rect().height();
		let whole_area = |ui: &egui::Ui| -> Vec2 {
			/*vec2(
				ui.available_width(),
				//ui.available_height().min(window_height * 0.8)
				ui.available_height() * 0.8
			)*/
			ui.available_size()
		};

		match displayed {
			DisplayedModal::NewModGroup(new_group) => {
				modal.show(|ui| {
					modal.title(ui, "New ModGroup");

					modal.frame(ui, |ui| {
						ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
							ui.with_layout(Layout::left_to_right(Align::BOTTOM), |ui| {
								cancel_button = Some(modal.caution_button(ui, "Cancel"));

								ui.with_layout(Layout::right_to_left(Align::BOTTOM), |ui| {
									create_button = Some(modal.suggested_button(ui, "Create!"));
								});
							});

							ui.add_space(8.);

							ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
								ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
									ui.label("Name:");

									ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
										ui.text_edit_singleline(&mut new_group.wip.name);
									});
								});

								ui.add_space(8.);

								egui::ScrollArea::vertical().show(ui, |ui| {
									for (id, md) in &self.all_mods.mods {
										list_mod(
											ui,
											id,
											true,
											Some(md),
											&self.all_mods.mods,
											0,
											new_group
										);
									}
								});
							});
						});
					});
				});

				modal.open();

				if create_button.is_some_and(|b| b.clicked()) {
					let Some(DisplayedModal::NewModGroup(mut creating)) = self.modal.take() else {
						unreachable!(
							"This should only be clickable if we're already creating a new mod group"
						);
					};

					for key in creating.dependents.into_keys() {
						creating.wip.mods.insert(key);
					}

					match make_files_for_modgroup(
						&self.dirs.modgroups_dir,
						&self.dirs.mod_dir,
						creating.wip
					) {
						Ok(group) => _ = self.modgroups.insert(group),
						Err(ModGroupCreationErr { step, err }) => self
							.visible_errors
							.push(format!("Couldn't create modgroup: {step} ({err})",))
					}
				}

				if cancel_button.is_some_and(|b| b.clicked()) {
					self.modal = None;
				}
			}
			DisplayedModal::DeletingMod {
				to_delete,
				also_delete
			} => {
				let modd = self.all_mods.mods.get(to_delete).unwrap();

				// we're assuming you can only get to this point if it's actually ok to delete this
				// mod (i.e. it's not required by anything else)
				modal.show(|ui| {
					modal.title(ui, format!("Delete {}?", modd.name));

					modal.frame(ui, |ui| {
						ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
							ui.with_layout(Layout::left_to_right(Align::BOTTOM), |ui| {
								cancel_button = Some(modal.caution_button(ui, "Cancel"));

								ui.with_layout(Layout::right_to_left(Align::BOTTOM), |ui| {
									create_button = Some(modal.suggested_button(ui, "Delete!"));
								});
							});

							ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
								let in_any_groups = self.modgroups.iter().any(|g| g.mods.contains(to_delete));
								if in_any_groups {
									ui.label("Warning: Deleting this mod will remove it from the following modgroups as well:");
									for group in &self.modgroups {
										if !group.mods.contains(to_delete) {
											continue;
										}

										ui.horizontal(|ui| {
											ui.add_space(10.);
											ui.label(&group.name);
										});
									}

									ui.add_space(20.);
								}

								if modd.dependencies.is_empty() {
									ui.label("Are you sure you want to delete this mod?");
								} else {
									ui.label(format!("This mod requires some other mods that you won't need to keep around anymore once this mod is gone. Select which ones you also want to delete along with {}", modd.name));

									egui::ScrollArea::vertical().show(ui, |ui| {
										list_mod_as_dependent_to_delete(ui, 0, modd, also_delete, &self.all_mods);
									});
								}
							});
						});
					});
				});

				modal.open();

				if create_button.is_some_and(|b| b.clicked()) {
					let mut got_err = false;

					for id in also_delete.iter().chain(std::iter::once(&*to_delete)) {
						if let Err(e) = delete_mod(to_delete, &self.dirs, &self.modgroups) {
							got_err = true;
							self.visible_errors
								.push(format!("Can't delete mod with id {id}: {e}"));
						}
					}

					if !got_err {
						self.modal = None;
					}
				}

				if cancel_button.is_some_and(|b| b.clicked()) {
					self.modal = None;
				}
			}
		}
	}

	fn spawn_browser(&mut self) {
		match self.browser.take() {
			// drop the receiver, we won't need it once we abort the task
			Some(BrowserSession { task, receiver: _ }) => {
				task.abort();
			}
			None => {
				let (sender, receiver) = mpsc::channel();

				let task = self.runtime.spawn(async move {
					if let Err(e) = launch_browser(&sender).await {
						println!("got err: {e}");
						_ = sender.send(BrowserMessage::Error(format!(
							"Couldn't launch browser: {e}"
						)));
					}
				});
				self.browser = Some(BrowserSession { task, receiver });
			}
		}
	}
}

fn list_mod(
	ui: &mut egui::Ui,
	mod_id: &UniqueId,
	required: bool,
	already_found: Option<&Mod>,
	all_mods: &BTreeMap<UniqueId, Mod>,
	indent_level: u16,
	new_group: &mut NewModGroup
) {
	let depended_upon = new_group
		.dependents
		.get(mod_id)
		.is_some_and(|set| !set.is_empty());
	let contains = new_group.wip.mods.contains(mod_id) || depended_upon;
	let selectable = !depended_upon && indent_level == 0;
	let found_mod = already_found.or_else(|| all_mods.get(mod_id));

	if found_mod.is_none() && !required {
		return;
	}

	let mut show_dependencies = false;
	let has_deps_to_show = found_mod.is_some_and(|m| !m.dependencies.is_empty());

	ui.horizontal(|ui| {
		ui.add_space(f32::from(indent_level) * 10.);
		ui.add_enabled_ui(selectable, |ui| match found_mod {
			Some(modd) =>
				_ = ui.horizontal(|ui| {
					if ui
						.checkbox(&mut { contains }, modd.user_visible_name(all_mods))
						.clicked()
					{
						if contains {
							new_group.wip.mods.remove(&modd.unique_id);
							remove_mod_and_dependencies(modd, all_mods, &mut new_group.dependents);
						} else {
							new_group.wip.mods.insert(modd.unique_id.clone());
							add_mod_and_dependencies(modd, all_mods, &mut new_group.dependents);
						}
					}

					if indent_level == 0 && has_deps_to_show {
						ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
							if new_group.dependencies_expanded.contains(mod_id) {
								show_dependencies = true;
								if ui.button("⏷").clicked() {
									new_group.dependencies_expanded.remove(mod_id);
								}
							} else if ui.button("⏵").clicked() {
								new_group.dependencies_expanded.insert(mod_id.clone());
							}
						});
					}
				}),
			None => _ = ui.label(format!("ERROR: Dependency with id {mod_id} not found"))
		});
	});

	if show_dependencies && let Some(found) = found_mod {
		for dependency in &found.dependencies {
			list_mod(
				ui,
				&dependency.unique_id,
				dependency.is_required,
				None,
				all_mods,
				indent_level + 1,
				new_group
			);
		}
	}
}

fn list_mod_as_dependent_to_delete(
	ui: &mut egui::Ui,
	indent_level: u16,
	modd: &Mod,
	also_delete: &mut BTreeSet<UniqueId>,
	all_mods: &AllMods
) {
	for dependency in &modd.dependencies {
		let installed = all_mods.mods.get(&dependency.unique_id);
		let has_dependents = all_mods
			.dependents
			.get(&dependency.unique_id)
			.is_some_and(|d| !d.is_empty());

		let (Some(installed), true) = (installed, has_dependents) else {
			continue;
		};

		let is_set_to_delete = also_delete.contains(&dependency.unique_id);
		ui.horizontal_wrapped(|ui| {
			ui.add_space(10. * f32::from(indent_level));

			if ui
				.checkbox(
					&mut { is_set_to_delete },
					installed.user_visible_name(&all_mods.mods)
				)
				.clicked()
			{
				if is_set_to_delete {
					also_delete.remove(&dependency.unique_id);
				} else {
					also_delete.insert(dependency.unique_id.clone());
				}
			}
		});

		list_mod_as_dependent_to_delete(ui, indent_level + 1, installed, also_delete, all_mods);
	}
}

fn remove_mod_and_dependencies(
	modd: &Mod,
	all_mods: &BTreeMap<UniqueId, Mod>,
	dependents: &mut BTreeMap<UniqueId, BTreeSet<UniqueId>>
) {
	for set_of_dependents in dependents.values_mut() {
		_ = set_of_dependents.remove(&modd.unique_id);
	}

	for dependency in &modd.dependencies {
		if let Some(modd) = all_mods.get(&dependency.unique_id) {
			remove_mod_and_dependencies(modd, all_mods, dependents);
		}
	}
}

fn add_mod_and_dependencies(
	modd: &Mod,
	all_mods: &BTreeMap<UniqueId, Mod>,
	dependents: &mut BTreeMap<UniqueId, BTreeSet<UniqueId>>
) {
	for dependency in &modd.dependencies {
		match dependents.get_mut(&dependency.unique_id) {
			Some(deps) => _ = deps.insert(modd.unique_id.clone()),
			None =>
				_ = dependents.insert(
					dependency.unique_id.clone(),
					BTreeSet::from_iter([modd.unique_id.clone()])
				),
		}

		if let Some(modd) = all_mods.get(&dependency.unique_id) {
			add_mod_and_dependencies(modd, all_mods, dependents);
		}
	}
}

fn make_dir_symlink(original: &Path, link: &Path) -> std::io::Result<()> {
	if std::fs::exists(link)? {
		let points_to = read_link(link)?;

		if points_to != original {
			return Err(std::io::Error::new(
				ErrorKind::AlreadyExists,
				format!(
					"The provided link already points to {points_to:?} (which is not the new directory that was requested"
				)
			));
		}

		return Ok(());
	};

	#[cfg(unix)]
	{
		std::os::unix::fs::symlink(original, link)
	}

	#[cfg(windows)]
	{
		std::os::windows::fs::symlink_dir(original, link)
	}
}

fn start_discovering_new_mods_on_fs(runtime: &Runtime) -> Option<Receiver<AppMsg>> {
	let paths = rfd::FileDialog::new()
		.set_can_create_directories(true)
		.pick_folders()?;

	let (sender, receiver) = channel();

	// we could make this generic so we can make a version that doesn't accumulate at all
	for path in paths {
		let mut accum = BTreeSet::default();
		let sender = sender.clone();
		runtime.spawn(async move {
			crate::mod_group::collect_mods_in_path(&path, &sender, &mut accum).await
		});
	}

	Some(receiver)
}

enum CopyDirError {
	ReadDir { dir: Box<Path> },
	DirChildIsErr { parent: Box<Path> },
	NoFileType { item_path: Box<Path> },
	CopyFile { from: Box<Path>, to: Box<Path> },
	MakeChildDir { new_dir: Box<Path> }
}

impl Display for CopyDirError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::ReadDir { dir } => write!(
				f,
				"Couldn't read the contents of the original mod dir ({dir:?})"
			),
			Self::DirChildIsErr { parent } => write!(
				f,
				"Couldn't read the metadata of a file that exists inside {parent:?}"
			),
			Self::NoFileType { item_path } =>
				write!(f, "Couldn't get the filetype of the item at {item_path:?}"),
			Self::CopyFile { from, to } => write!(f, "Couldn't copy file from {from:?} to {to:?}"),
			Self::MakeChildDir { new_dir } =>
				write!(f, "Couldn't make new directory for mod at {new_dir:?}"),
		}
	}
}

fn copy_contents_of_dir_into_new_dir(
	orig_parent: &Path,
	new_parent: &Path
) -> Result<(), (CopyDirError, std::io::Error)> {
	let dir = std::fs::read_dir(orig_parent).map_err(|e| {
		(
			CopyDirError::ReadDir {
				dir: orig_parent.into()
			},
			e
		)
	})?;

	for item in dir {
		let item = item.map_err(|e| {
			(
				CopyDirError::DirChildIsErr {
					parent: orig_parent.into()
				},
				e
			)
		})?;

		let ft = item.file_type().map_err(|e| {
			(
				CopyDirError::NoFileType {
					item_path: item.path().into()
				},
				e
			)
		})?;

		if ft.is_file() {
			let from = item.path().into();
			let to = new_parent.join(item.file_name()).into();
			std::fs::copy(&from, &to).map_err(|e| (CopyDirError::CopyFile { from, to }, e))?;
		} else if ft.is_dir() {
			let new_dir = new_parent.join(item.file_name()).into();
			std::fs::create_dir(&new_dir)
				.map_err(|e| (CopyDirError::MakeChildDir { new_dir }, e))?;
		} else if ft.is_symlink() {
			// we don't want to recurse into symlinks. They'll only exist in mod files if someone
			// messed with them, and if that's the case, we can't really bet on anything working.
		}
	}

	Ok(())
}

pub fn in_rect(
	ui: &mut egui::Ui,
	vec2: Vec2,
	layout: Layout,
	children: impl FnOnce(&mut egui::Ui)
) {
	let (id, rect) = ui.allocate_space(vec2);
	let builder = UiBuilder::new().id_salt(id).max_rect(rect).layout(layout);
	let mut new_ui = ui.new_child(builder);
	children(&mut new_ui);
}

pub fn split_horiz(
	ui: &mut egui::Ui,
	height: f32,
	left: impl FnOnce(&mut egui::Ui),
	right: impl FnOnce(&mut egui::Ui)
) {
	let width = ui.available_width() / 2.;
	ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
		let orig = ui.spacing().item_spacing.x;
		ui.spacing_mut().item_spacing.x = 0.;
		in_rect(
			ui,
			vec2(width, height),
			Layout::top_down(Align::LEFT),
			|ui| {
				ui.spacing_mut().item_spacing.x = orig;
				left(ui);
			}
		);
		in_rect(
			ui,
			vec2(width, height),
			Layout::top_down(Align::LEFT),
			|ui| {
				ui.spacing_mut().item_spacing.x = orig;
				right(ui);
			}
		);
	});
}

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
	mod_group::{Mod, ModGroup, UniqueId, collect_mods_in_path},
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
	all_mods: BTreeMap<UniqueId, Mod>,
	modgroups: BTreeSet<ModGroup>,
	visible_errors: Vec<String>,
	receiver: Receiver<AppMsg>,
	current_run: Option<RunningDisplay>,
	creating: Option<ModGroup>,
	discovering_new_mods_from: Option<Receiver<AppMsg>>,
	config: FileConfig,
	browser: Option<BrowserSession>
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
			all_mods: BTreeMap::default(),
			// this'll also be populated as the rayon stuff runs
			modgroups: BTreeSet::default(),
			visible_errors,
			receiver,
			current_run: None,
			creating: None,
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

		self.check_if_discovered_new_mod();

		if let Some(current) = &mut self.creating {
			let modal = egui_modal::Modal::new(ctx, "new_modgroup_modal");

			let mut create_button = None;
			let mut cancel_button = None;
			modal.show(|ui| {
				modal.title(ui, "New ModGroup");

				modal.frame(ui, |ui| {
					ui.text_edit_singleline(&mut current.name);

					ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
						egui::ScrollArea::vertical().show(ui, |ui| {
							for (id, md) in &self.all_mods {
								let contains = current.mods.contains(id);

								// i essentially want to make an immutable bool here 'cause it's easier to
								// follow the logic if we don't change `contains`.
								if ui
									.checkbox(
										&mut { contains },
										format!("{} ({})", md.name, md.version)
									)
									.clicked()
								{
									if contains {
										current.mods.remove(id);
									} else {
										current.mods.insert(id.clone());
									}
								}
							}
						})
					});

					ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
						create_button = Some(modal.suggested_button(ui, "Create!"));

						ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
							cancel_button = Some(modal.button(ui, "Cancel"));
						});
					});
				});
			});

			modal.open();

			if create_button.is_some_and(|b| b.clicked()) {
				match make_files_for_modgroup(
					&self.dirs.modgroups_dir,
					&self.dirs.mod_dir,
					self.creating.take().unwrap()
				) {
					Ok(group) => _ = self.modgroups.insert(group),
					Err(ModGroupCreationErr { step, err }) => self
						.visible_errors
						.push(format!("Couldn't create modgroup: {step} ({err})",))
				}
			}

			if cancel_button.is_some_and(|b| b.clicked()) {
				self.creating = None;
			}
		}

		egui::CentralPanel::default().show(ctx, |ui| {
			ui.with_layout(Layout::top_down(Align::Min), |ui| {
				ui.horizontal(|ui| {
					ui.heading("Prismatic");
					ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
						/*ui.add_enabled_ui(self.browser.is_none(), |ui| {
							if ui.button("Log In!").clicked() {
								self.spawn_browser();
							}
						});*/

						ui.add_enabled_ui(self.creating.is_none(), |ui| {
							if ui.button("+ New ModGroup").clicked() {
								self.creating = Some(ModGroup::default());
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
						logs_displayed: true,
						instance
					}) => {
						ui.heading(format!("Currently running ModGroup '{}'", instance.name));

						ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
							ui.horizontal(|ui| {
								if ui.button("Restart").clicked() {
									if let Err(e) = instance.child.kill() {
										self.visible_errors.push(format!(
											"Couldn't kill currently-running stardew: {e}"
										));
									};
									new_run = match RunningInstance::try_new(
										&instance.name,
										&self.config.smapi_config,
										self.dirs.stardew_paths.first().unwrap(),
										&self.dirs.modgroups_dir
									) {
										Ok(instance) => Some(RunningDisplay {
											logs_displayed: true,
											instance
										}),
										Err(e) => {
											self.visible_errors.push(format!(
												"Couldn't start stardew back up again: {e}"
											));
											None
										}
									};
								} else if ui.button("Stop Game").clicked()
									&& let Err(e) = instance.child.kill()
								{
									self.visible_errors
										.push(format!("Couldn't kill stardew: {e}"));
								}
							});

							in_rect(
								ui,
								ui.available_size(),
								Layout::top_down(Align::LEFT),
								|ui| {
									egui::ScrollArea::vertical().id_salt("logs_buf").show(
										ui,
										|ui| {
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
													_ = ui.label(format!(
														"Process exited with code {code}"
													)),
												Ok(None) => () // it's still running, continue
											}
										}
									);
								}
							);
						});
					}
					_ => self.mods_and_modgroup_area(ui)
				}

				if let Some(new_run) = new_run {
					self.current_run = Some(new_run);
				}

				egui::ScrollArea::vertical()
					.stick_to_bottom(true)
					.auto_shrink(Vec2b::TRUE)
					.show(ui, |ui| {
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
					for m in self.all_mods.values() {
						ui.label(format!("{} by {}, version {}", m.name, m.author, m.version));
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
		all_mods: &mut BTreeMap<UniqueId, Mod>,
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
				_ = all_mods.insert(m.unique_id.clone(), m)
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

enum FailedCreation {
	ModGroupFolderCreation(Box<Path>),
	CantCheckIfModExists {
		mod_id: UniqueId,
		expected_at: Box<Path>
	},
	NoSuchMod {
		mod_id: UniqueId,
		expected_at: Box<Path>
	},
	ModSymlink {
		mod_id: UniqueId,
		found_at: Box<Path>,
		link_to: Box<Path>
	}
}

impl Display for FailedCreation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::ModGroupFolderCreation(path) =>
				write!(f, "couldn't create main folder for modgroup at {path:?}"),
			Self::CantCheckIfModExists {
				mod_id,
				expected_at
			} => write!(
				f,
				"couldn't check if mod with id {mod_id} actually exists (we expect it at {expected_at:?})"
			),
			Self::NoSuchMod {
				mod_id,
				expected_at
			} => write!(
				f,
				"selected mod '{mod_id}' doesn't actually seem to exist (expected to see it at {expected_at:?} - did you delete it from the filesystem?"
			),
			Self::ModSymlink {
				mod_id,
				found_at,
				link_to
			} => write!(
				f,
				"couldn't make symlink from {found_at:?} to {link_to:?} to associate mod {mod_id} with this modgroup"
			)
		}
	}
}

struct ModGroupCreationErr {
	step: FailedCreation,
	err: std::io::Error
}

fn make_files_for_modgroup(
	modgroup_dir: &Path,
	mod_dir: &Path,
	group: ModGroup
) -> Result<ModGroup, ModGroupCreationErr> {
	let ModGroup { name, mods } = group;

	let modgroup_path = modgroup_dir.join(&name);

	if let Err(err) = std::fs::create_dir(&modgroup_path) {
		return Err(ModGroupCreationErr {
			step: FailedCreation::ModGroupFolderCreation(modgroup_path.into()),
			err
		});
	}

	let do_the_rest = || {
		let mods = mods
			.into_iter()
			.map(|id| {
				let real_dir = mod_dir.join(&id.0);

				match std::fs::exists(&real_dir) {
					Err(e) =>
						return Err(ModGroupCreationErr {
							step: FailedCreation::CantCheckIfModExists {
								mod_id: id,
								expected_at: real_dir.into()
							},
							err: e
						}),
					Ok(false) =>
						return Err(ModGroupCreationErr {
							step: FailedCreation::NoSuchMod {
								mod_id: id,
								expected_at: real_dir.into()
							},
							err: std::io::Error::new(
								std::io::ErrorKind::NotFound,
								"No such mod folder exists"
							)
						}),
					// if it exists, all is good
					Ok(true) => ()
				}

				let link = modgroup_path.join(&id.0);
				match make_dir_symlink(&real_dir, &link) {
					Err(err) => Err(ModGroupCreationErr {
						step: FailedCreation::ModSymlink {
							mod_id: id,
							found_at: real_dir.into(),
							link_to: link.into()
						},
						// TODO: Should we also include `real_dir` into this path somehow?
						err
					}),
					Ok(()) => Ok(id)
				}
			})
			.collect::<Result<BTreeSet<_>, _>>()?;

		Ok(ModGroup { mods, name })
	};

	// if we fail when creating the other symlinks or whatever, make sure to clean up after
	// ourselves.
	do_the_rest().inspect_err(|_| _ = std::fs::remove_dir_all(modgroup_path))
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

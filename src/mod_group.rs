use core::{borrow::Borrow, fmt::Display, str::FromStr};
use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet},
	path::Path,
	sync::mpsc::Sender
};

use serde::Deserialize;

use crate::{AppMsg, dirs::Dirs, make_dir_symlink};

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileRepresentableMod {
	name: String,
	author: String,
	version: String,
	description: String,
	#[serde(rename = "UniqueID")]
	unique_id: Option<String>,
	minimum_api_version: Option<String>,
	#[serde(default)]
	update_keys: Vec<String>,
	content_pack_for: Option<ContentPack>,
	#[serde(default)]
	dependencies: Vec<FileDependency>
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct ContentPack {
	#[serde(rename = "UniqueID")]
	pub unique_id: String
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FileDependency {
	#[serde(rename = "UniqueID")]
	unique_id: String,
	is_required: Option<bool>
}

#[derive(Debug)]
pub struct Dependency {
	pub unique_id: UniqueId,
	pub is_required: bool
}

#[derive(Debug)]
pub struct Mod {
	pub name: String,
	pub author: String,
	pub version: MaybeSemver,
	description: String,
	pub unique_id: UniqueId,
	minimum_api_version: Option<MaybeSemver>,
	update_keys: Vec<UpdateKey>,
	pub content_pack_for: Option<ContentPack>,
	pub dependencies: Vec<Dependency>,
	pub manifest_path: Box<Path>
}

impl Mod {
	async fn read_from(manifest_path: Box<Path>) -> Result<Self, (std::io::Error, Box<Path>)> {
		let json_data = match tokio::fs::read_to_string(&manifest_path).await {
			Ok(d) => d,
			Err(e) =>
				return Err((
					std::io::Error::new(
						e.kind(),
						format!("Couldn't read manifest.json file at {manifest_path:?}: {e}")
					),
					manifest_path
				)),
		};

		let FileRepresentableMod {
			name,
			author,
			version,
			description,
			unique_id,
			minimum_api_version,
			update_keys,
			content_pack_for,
			dependencies
		} = match json5::from_str(&json_data) {
			Ok(fm) => fm,
			Err(e) =>
				return Err((
					std::io::Error::other(format!(
						"Couldn't deserialize data at {manifest_path:?} to json: {e}"
					)),
					manifest_path
				)),
		};

		// We have to do this manual `map` because we are moving `manifest_path` into the Error
		// returned and doing that in a closure isn't very friendly to the borrow checker
		let mut update_keys_parsed = Vec::with_capacity(update_keys.len());
		for key in update_keys {
			update_keys_parsed.push(match UpdateKey::from_str(&key) {
				Err(e) =>
					return Err((
						std::io::Error::other(format!(
							"Couldn't parse update key from {key:?}: {e}"
						)),
						manifest_path
					)),
				Ok(k) => k
			});
		}

		let unique_id = match (unique_id, &*name, &*author) {
			// this is a special exception - these have no uniqueId
			(None, "Console Commands", "SMAPI") => "SMAPI.ConsoleCommands".to_string(),
			(None, "SaveBackup", "SMAPI") => "SMAPI.SaveBackup".to_string(),
			(None, _, _) =>
				return Err((
					std::io::Error::other("Missing required field 'UniqueID'"),
					manifest_path
				)),
			(Some(id), _, _) => id
		};

		Ok(Self {
			name,
			author,
			version: MaybeSemver::from(version),
			description,
			unique_id: UniqueId(unique_id),
			minimum_api_version: minimum_api_version.map(MaybeSemver::from),
			update_keys: update_keys_parsed,
			content_pack_for,
			dependencies: dependencies
				.into_iter()
				.map(
					|FileDependency {
					     unique_id,
					     is_required
					 }| Dependency {
						unique_id: UniqueId(unique_id),
						is_required: is_required.unwrap_or(true)
					}
				)
				.collect(),
			manifest_path
		})
	}

	pub fn user_visible_name(&self, other_mods: &BTreeMap<UniqueId, Mod>) -> String {
		let mut ret = format!("{} (v{}", self.name, self.version);

		if let Some(pack_for) = &self.content_pack_for {
			ret.push_str(", content pack for ");
			match other_mods.get(&pack_for.unique_id) {
				Some(modd) => ret.push_str(&modd.name),
				None => ret.push_str(&pack_for.unique_id)
			}
		}

		ret.push(')');
		ret
	}
}

#[derive(Clone, PartialEq, PartialOrd, Ord, Eq, Debug, Deserialize)]
pub struct UniqueId(pub String);

impl Display for UniqueId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.0.fmt(f)
	}
}

impl Borrow<String> for UniqueId {
	fn borrow(&self) -> &String {
		&self.0
	}
}

#[derive(Debug)]
pub enum MaybeSemver {
	Not(String),
	Semver(semver::Version)
}

impl Display for MaybeSemver {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Not(s) => s.fmt(f),
			Self::Semver(v) => v.fmt(f)
		}
	}
}

impl From<String> for MaybeSemver {
	fn from(value: String) -> Self {
		semver::Version::from_str(&value).map_or(MaybeSemver::Not(value), MaybeSemver::Semver)
	}
}

/// `Variant(None)` just means that we don't know what their update key is - not that they don't have
/// one. `Nexus(None) != Nexus(None)`
#[derive(Debug)]
enum UpdateKey {
	Nexus(Option<u64>),
	ModDrop(Option<u64>),
	GitHub { user: String, repo: String }
}

impl FromStr for UpdateKey {
	type Err = Cow<'static, str>;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let mut parts = s.split(':');

		let Some(provider) = parts.next() else {
			return Err("No provider (e.g. 'Nexus') found".into());
		};

		let Some(id) = parts.next() else {
			return Err("No id (e.g. '1234' in 'Nexus:1234') found after the provider".into());
		};

		let wip = match provider {
			"Nexus" => Self::Nexus(None),
			"Moddrop" | "ModDrop" => Self::ModDrop(None),
			"GitHub" => {
				let mut id_split = id.split('/');

				let user = id_split
					.next()
					.ok_or(Cow::Borrowed(
						"Update provider 'GitHub' requires a user and repo, but this contained no user"
					))?
					.to_string();

				let repo = id_split
					.next()
					.ok_or(Cow::Borrowed(
						"Update provider 'GitHub' requires a user and repo, but this contained no repo"
					))?
					.to_string();

				return Ok(Self::GitHub { user, repo });
			}
			other => return Err(format!("UpdateKey contained unknown provider {other:?}").into())
		};

		let id = id.parse().ok();

		Ok(match wip {
			Self::Nexus(_) => Self::Nexus(id),
			Self::ModDrop(_) => Self::ModDrop(id),
			key @ Self::GitHub { .. } => key
		})
	}
}

/// Each modgroup, on disk, is just a directory with a bunch of symlinks in it. Each symlink points
/// to the actual directory of a mod. We don't store the modgroups in the Stardew Valley directory
/// by themselves - instead we create a folder called `PrismaticModGroups` inside the SDV directory
/// that stores symlinks to all the actual directories of the mod groups. This just makes it easier
/// for us to ensure that nobody else is actually touching the content of the mod groups (or
/// accidentally deleting them) since the actual groups are stored in a separate directory
#[derive(PartialOrd, Ord, PartialEq, Eq, Default)]
pub struct ModGroup {
	pub name: String,
	pub mods: BTreeSet<UniqueId>
}

// TODO: Refactor to be non-recursive to avoid all the currently-necessary `Box::pins` (due to not
// being able to natively represent recursive functions)
//
// TODO: Handle a situation where we may discover a mod before its dependency, and then all the
// mods are present by the dependents graph is messed up (and then they may accidentally be allowed
// to delete a mod that is actually required. maybe)
pub async fn collect_mods_in_path(
	path: &Path,
	sender: &Sender<AppMsg>,
	id_accum: &mut BTreeSet<UniqueId>
) {
	let path_depth = path.components().count();
	if path_depth > 255 {
		_ = sender.send(AppMsg::UserRelevantError(format!(
			"Gave up on detecting mods at {path:?} due to passing the permitted path depth of 255; have you accidentally setup infinitely recursive symlinks?"
		)));
		return;
	}

	let Ok(mut dir) = tokio::fs::read_dir(path).await.inspect_err(|e| {
		_ = sender.send(AppMsg::UserRelevantError(format!(
			"Couldn't check for mods inside {path:?}: {e}"
		)))
	}) else {
		return;
	};

	loop {
		let entry = match dir.next_entry().await {
			Err(e) => {
				_ = sender.send(AppMsg::UserRelevantError(format!(
					"Couldn't check for mods inside {path:?}: {e}"
				)));
				continue;
			}
			Ok(None) => break,
			Ok(Some(entry)) => entry
		};

		let Ok(ft) = entry.file_type().await.inspect_err(|e| {
			_ = sender.send(AppMsg::UserRelevantError(format!(
				"Couldn't detect filetype of {:?} (needed to determine if it's a mod file or not): {e}",
				entry.path()
			)))
		}) else {
			continue;
		};

		let path = entry.path();
		if ft.is_file() {
			parse_if_path_is_manifest(path.into(), sender, id_accum).await;
		} else if ft.is_symlink() {
			Box::pin(recurse_symlink(&path, sender, id_accum)).await;
		} else if ft.is_dir() {
			Box::pin(collect_mods_in_path(&path, sender, id_accum)).await;
		}
	}
}

async fn recurse_symlink(path: &Path, sender: &Sender<AppMsg>, id_accum: &mut BTreeSet<UniqueId>) {
	let Ok(resolved) = tokio::fs::read_link(path).await.inspect_err(|e| {
		_ = sender.send(AppMsg::UserRelevantError(format!(
			"Couldn't follow symlink at {path:?}: {e}"
		)))
	}) else {
		return;
	};

	match tokio::fs::metadata(&resolved).await {
		Err(e) => _ = sender.send(AppMsg::UserRelevantError(format!(
			"Couldn't get metadata of file at {resolved:?} to check if it's a mod file: {e}"
		))),
		Ok(stat) if stat.is_file() => parse_if_path_is_manifest(resolved.into(), sender, id_accum).await,
		Ok(stat) if stat.is_dir() => collect_mods_in_path(path, sender, id_accum).await,
		Ok(stat) if stat.is_symlink() => Box::pin(recurse_symlink(&resolved, sender, id_accum)).await,
		// if it's not a file, directory, or symlink, then I have no idea how to handle it. Just
		// send an error
		Ok(stat) => _ = sender.send(AppMsg::UserRelevantError(format!(
			"Don't know how to handle file at {resolved:?} which is not a file, directory, or link (got stat {stat:?})"
		)))
	}
}

async fn parse_if_path_is_manifest(
	path: Box<Path>,
	sender: &Sender<AppMsg>,
	id_accum: &mut BTreeSet<UniqueId>
) {
	if path.file_name().is_none_or(|p| p != "manifest.json") {
		return;
	}

	_ = match Mod::read_from(path).await {
		Ok(m) => {
			id_accum.insert(m.unique_id.clone());
			sender.send(AppMsg::ModDiscovered(m))
		}
		Err((e, path)) => sender.send(AppMsg::UserRelevantError(format!(
			"Couldn't read mod at {path:?}: {e}"
		)))
	};
}

pub enum FailedCreation {
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

pub struct ModGroupCreationErr {
	pub step: FailedCreation,
	pub err: std::io::Error
}

pub fn make_files_for_modgroup(
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

pub fn delete_mod(
	modd: &UniqueId,
	dirs: &Dirs,
	groups: &BTreeSet<ModGroup>
) -> Result<(), std::io::Error> {
	let mod_path = dirs.mod_dir.join(&modd.0);

	std::fs::remove_dir_all(mod_path)?;

	for group in groups.iter().filter(|g| g.mods.contains(modd)) {
		let link = dirs.modgroups_dir.join(&group.name).join(&modd.0);

		// docs says this removes a symlink if it exists, and that's what we want.
		std::fs::remove_dir_all(&link)?;
	}

	Ok(())
}

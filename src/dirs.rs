use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
	sync::mpsc::Sender
};

use crate::{
	APP_NAME, AppMsg,
	mod_group::{ModGroup, collect_mods_in_path}
};

pub struct Dirs {
	pub log_dir: PathBuf,
	pub mod_dir: PathBuf,
	pub modgroups_dir: PathBuf,
	// directories that contain the `StardewValley` executable
	pub stardew_paths: BTreeSet<PathBuf>
}

impl Default for Dirs {
	fn default() -> Self {
		let data_dir = ::dirs::data_local_dir()
			.expect("Could not find directory for logs - is $XDG_DATA_DIR set?");
		let app_data_dir = data_dir.join(APP_NAME);
		let log_dir = app_data_dir.join("logs");
		let mod_dir = app_data_dir.join("mods");
		let modgroups_dir = app_data_dir.join("modgroups");

		let mut dirs = Self {
			log_dir,
			mod_dir,
			modgroups_dir,
			stardew_paths: BTreeSet::default()
		};

		dirs.push_stardew_parent_if_path_exists(data_dir.clone(), &[
			"Steam",
			"steamapps",
			"common",
			"Stardew Valley",
			"StardewValley"
		]);

		dirs
	}
}

impl Dirs {
	fn push_stardew_parent_if_path_exists(&mut self, mut root: PathBuf, parts: &[&str]) {
		for p in parts {
			root.push(p);
		}

		match std::fs::exists(&root) {
			// I guess if this does return something, that's technically a bug, but a very
			// inconsequential one.
			Ok(true) =>
				if let Some(parent) = root.parent() {
					self.stardew_paths.insert(parent.into());
				},
			Ok(false) => (),
			Err(_) => {
				// TODO: Log
			}
		}
	}
}

pub async fn get_modgroups_from_data_dir(modgroup_dir: &Path, sender: &Sender<AppMsg>) {
	let Ok(mut dir) = tokio::fs::read_dir(modgroup_dir).await.inspect_err(|e| {
		sender
			.send(AppMsg::UserRelevantError(format!(
				"Couldn't read the list of modgroups in {modgroup_dir:?} due to: {e}"
			)))
			.unwrap()
	}) else {
		return;
	};

	loop {
		let entry = match dir.next_entry().await {
			Err(e) => {
				_ = sender.send(AppMsg::UserRelevantError(format!(
					"Couldn't read a specific item or directory while looking for modgroups in in {modgroup_dir:?}: {e}"
				)));
				continue;
			}
			Ok(None) => break,
			Ok(Some(e)) => e
		};

		let Ok(ft) = entry.file_type().await.inspect_err(|e| {
			_ = sender.send(AppMsg::UserRelevantError(format!(
				"Couldn't check filetype of what should be a modgroup directory at {:?}: {e}",
				entry.path()
			)))
		}) else {
			continue;
		};

		if ft.is_dir() {
			let file_name = entry.file_name();
			let Some(modgroup_name) = file_name.to_str() else {
				if sender.send(AppMsg::UserRelevantError(format!(
					"Couldn't detect mods inside directory with non-utf8 name '{:?}'", entry.path()
				))).is_err() {
					return;
				}
				continue;
			};
			let mut list_of_mods = BTreeSet::default();

			collect_mods_in_path(&entry.path(), sender, &mut list_of_mods).await;

			let was_err = sender.send(AppMsg::ModGroupDiscovered(ModGroup {
				name: modgroup_name.to_string(),
				mods: list_of_mods
			})).is_err();

			if was_err {
				return;
			}
		} else if sender.send(AppMsg::UserRelevantError(format!(
			"Found non-directory '{:?}' inside modgroup directory (all top-level items in the modgroup directory should be modgroups",
			entry.path()
		))).is_err() {
			return;
		}
	}
}

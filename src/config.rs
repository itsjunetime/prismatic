use core::str::FromStr;
use std::path::Path;

use kdl::{KdlDocument, KdlValue};

// config options that you can set in the config file
#[derive(Default, Debug)]
pub struct FileConfig {
	session_cookie: Option<String>,
	api_key: Option<String>,
	mod_detection: NewModDetectionConfig,
	pub smapi_config: SmapiConfig
}

#[derive(Default, Debug)]
pub struct NewModDetectionConfig {
	delete_after_copy: bool
}

#[derive(Default, Debug)]
pub struct SmapiConfig {
	// replaces:
	// - $SMAPI_EXECUTABLE with the full path to the SMAPI executable
	//
	// maybe someday we can support (but we don't support right now):
	// - $SDV_EXECUTABLE with the full path to the SMAPI executable
	// - $MODGROUP_NAME with the name of the modgroup
	// - $MODGROUP_PATH with the full path to the folder containing the mods in the modgroup
	pub custom_command: Option<(String, Vec<String>)>,
	pub custom_env: Option<Vec<EnvVar>>
}

#[derive(Debug)]
pub enum EnvVar {
	Remove { key: String },
	Add { key: String, value: String }
}

impl FileConfig {
	pub fn from_path(path: &Path) -> Result<Self, String> {
		let file_contents = std::fs::read_to_string(path)
			.map_err(|e| format!("Couldn't read file at '{path:?}': {e}"))?;

		let doc = KdlDocument::from_str(&file_contents)
			.map_err(|e| format!("Couldn't parse KDL from file contents: {e}"))?;

		let mut wip = Self::default();

		if let Some(node) = doc.get_arg("session_cookie") {
			match node {
				KdlValue::String(s) => wip.session_cookie = Some(s.to_string()),
				_ => return Err(format!("`session_cookie` must be a string (got {node:?})"))
			}
		}

		if let Some(node) = doc.get_arg("api_key") {
			match node {
				KdlValue::String(s) => wip.api_key = Some(s.to_string()),
				_ => return Err(format!("`api_key` must be a string (got {node:?})"))
			}
		}

		if let Some(node) = doc.get("mod_detection")
			&& let Some(delete_after_copy) = node.get("delete_after_copy")
		{
			match delete_after_copy {
				KdlValue::Bool(val) => wip.mod_detection.delete_after_copy = *val,
				_ =>
					return Err(format!(
						"`mod_detection.delete_after_copy` must be a boolean value (either `true` or `false`) - found {delete_after_copy:?}"
					)),
			}
		}

		if let Some(doc) = doc.get("smapi_config").and_then(|node| node.children()) {
			if let Some(custom_command) = doc.get("custom_command") {
				let mut entries = custom_command.entries().iter().map(|e| {
					if let Some(name) = e.name() {
						return Err(format!("Arguments for `custom_command` are not allowed to have names (but we found one named '{name}'"));
					}

					match e.value() {
						KdlValue::String(s) => Ok(s.to_string()),
						val => Err(format!("All arguments for `custom_command` must be strings, but we found a `{val}`"))
					}
				}).collect::<Result<Vec<_>, _>>()?;

				if entries.is_empty() {
					return Err("`custom_command` can't be empty - you must have at least one argument to execute".to_string());
				}

				wip.smapi_config.custom_command = Some((entries.remove(0), entries));
			}

			if let Some(env_node) = doc.get("custom_env") {
				let entries = env_node
					.entries()
					.iter()
					.map(|e| {
						let val = e.value();
						let Some(key) = e.name() else {
							return Err(format!(
								"No name provided for custom env entry with value '{val}'"
							));
						};

						let key = key.value().to_string();

						Ok(match val {
							KdlValue::Null => EnvVar::Remove { key },
							_ => EnvVar::Add {
								key,
								value: val.to_string()
							}
						})
					})
					.collect::<Result<Vec<_>, _>>()?;

				wip.smapi_config.custom_env = Some(entries);
			}
		}

		println!("got full config {wip:#?}");

		Ok(wip)
	}
}

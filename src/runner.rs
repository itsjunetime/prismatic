use std::{
	ffi::OsString,
	io::ErrorKind,
	path::Path,
	process::{Child, Command},
	sync::{Arc, Mutex, PoisonError},
	thread::JoinHandle
};

use crate::{
	config::{EnvVar, SmapiConfig},
	make_dir_symlink
};

pub struct RunningInstance {
	pub child: Child,
	pub name: String,
	pub logs_buf: Arc<Mutex<Vec<u8>>>,
	_log_consumer_thread: JoinHandle<()>
}

impl Drop for RunningInstance {
	fn drop(&mut self) {
		drop(self.child.kill());
	}
}

#[derive(thiserror::Error, Debug)]
pub enum TryRunError {
	#[error("Couldn't setup mod group by linking {link_from:?} to {link_to:?}: {error}")]
	FailedToSetupModGroup {
		link_from: Box<Path>,
		link_to: Box<Path>,
		error: std::io::Error
	},
	#[error("Couldn't check if SMAPI is installed at {tried:?}: {error}")]
	FailedToCheckIfSMAPIInstalled {
		tried: Box<Path>,
		error: std::io::Error
	},
	#[error("Couldn't find SMAPI, which we expected to be installed at {tried:?}")]
	NoSMAPIInstalled { tried: Box<Path> },
	#[error(
		"The path to SMAPI contains non-unicode characters: {path:?} - if you're not on linux, this is a bug"
	)]
	SMAPIPathContainsNonUnicode { path: Box<Path> },
	#[error("We couldn't create a pipe over which to communicate with the child process: {inner}")]
	FailedPipeCreation { inner: std::io::Error },
	#[error(
		"We couldn't clone the pipe we already created to share it between stdout and stderr: {inner}"
	)]
	FailedPipeCloning { inner: std::io::Error },
	#[error("Failed to run command {all_args:?}: {error}")]
	CommandFailed {
		all_args: Vec<OsString>,
		error: std::io::Error
	}
}

impl RunningInstance {
	pub fn try_new(
		name: &str,
		config: &SmapiConfig,
		stardew_dir: &Path,
		modgroup_dir: &Path
	) -> Result<Self, TryRunError> {
		// if windows fails to setup symlink, we can run `start ms-settings:developers` with a regular
		// user to allow it

		const SDV_MODS_DIR_NAME: &str = "prismatic_mods";

		let link_from = stardew_dir.join(SDV_MODS_DIR_NAME);

		make_dir_symlink(modgroup_dir, &link_from).map_err(|error| {
			TryRunError::FailedToSetupModGroup {
				link_from: link_from.clone().into(),
				link_to: modgroup_dir.into(),
				error
			}
		})?;

		let smapi_path = stardew_dir.join("StardewModdingAPI");
		match std::fs::exists(&smapi_path) {
			Err(error) =>
				return Err(TryRunError::FailedToCheckIfSMAPIInstalled {
					tried: smapi_path.into(),
					error
				}),
			Ok(true) => (),
			Ok(false) =>
				return Err(TryRunError::NoSMAPIInstalled {
					tried: smapi_path.into()
				}),
		}

		let Some(smapi_path_str) = smapi_path.to_str() else {
			return Err(TryRunError::SMAPIPathContainsNonUnicode {
				path: smapi_path.into()
			});
		};

		let mut cmd = match config.custom_command.clone() {
			Some((mut cmd, mut args)) => {
				let run_var_replacements =
					|s: &mut String| *s = s.replace("$SMAPI_EXECUTABLE", smapi_path_str);

				run_var_replacements(&mut cmd);

				for arg in &mut args {
					run_var_replacements(arg);
				}

				let mut cmd = Command::new(cmd);
				cmd.args(args);
				cmd
			}
			None => {
				let mods_path = format!("{SDV_MODS_DIR_NAME}/{name}");
				let mut cmd = Command::new(smapi_path);
				cmd.args(["--use-current-shell", "--mods-path"])
					.arg(&mods_path)
					.env("SMAPI_MODS_PATH", mods_path)
					.env("RUST_LOG", "debug");

				cmd
			}
		};

		if let Some(vars) = config.custom_env.as_ref() {
			for env in vars {
				_ = match env {
					EnvVar::Add { key, value } => cmd.env(key, value),
					EnvVar::Remove { key } => cmd.env_remove(key)
				}
			}
		}

		let (mut reader, writer) =
			std::io::pipe().map_err(|inner| TryRunError::FailedPipeCreation { inner })?;

		let err_writer = writer
			.try_clone()
			.map_err(|inner| TryRunError::FailedPipeCloning { inner })?;

		cmd.stdout(writer).stderr(err_writer);

		let all_args = std::iter::once(cmd.get_program())
			.chain(cmd.get_args())
			.map(OsString::from)
			.collect();

		println!("trying to run {all_args:?}");

		let child = cmd
			.spawn()
			.map_err(|error| TryRunError::CommandFailed { all_args, error })?;

		let logs_buf = Arc::new(Mutex::new(Vec::new()));

		let thread_buf = logs_buf.clone();
		let log_consumer_thread = std::thread::spawn(move || {
			use std::io::Read;

			// no particular reason for this size of buf. just for fun.
			let mut intermediate_buf = [0; 32];

			loop {
				match reader.read(&mut intermediate_buf) {
					// if it got interrupted, just continue...
					Err(e) if e.kind() == ErrorKind::Interrupted => (),
					// the process probably exited
					Err(_) => break,
					// if it read 0, then obvs we don't need to do anything
					Ok(0) => (),
					Ok(num_bytes @ 1..) => thread_buf
						.lock()
						.unwrap_or_else(PoisonError::into_inner)
						.extend(&intermediate_buf[..num_bytes])
				}
			}
		});

		Ok(Self {
			child,
			name: name.to_string(),
			logs_buf,
			_log_consumer_thread: log_consumer_thread
		})
	}
}

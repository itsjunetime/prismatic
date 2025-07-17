use std::sync::mpsc::Sender;

use chromiumoxide::{
	Browser, BrowserConfig,
	cdp::browser_protocol::{
		browser::{
			Bounds, GetWindowBoundsParams, GetWindowForTargetParams, SetWindowBoundsParams,
			WindowState
		},
		emulation::SetUserAgentOverrideParams,
		network::EventResponseReceived,
		target::CreateTargetParams
	}
};
use futures::StreamExt;
use serde_json::Value;

struct Client {
	// get from https://next.nexusmods.com/settings/api-keys
	api_key: String
}

const SDV_DOMAIN_NAME: &str = "stardewvalley";

struct ModFilesResponse {
	files: Vec<ModFile> // file_updates: Vec<FileUpdate>
}

struct ModFile {
	// first is the mod file id, second is the game id
	id: [u64; 2],
	// different from the mod file id
	uid: u64,
	// same as the first number in `id`
	file_id: u64,
	// E.g. `SMAPI 2.6 beta 16` - normally includes version info
	name: String,
	// E.g. `2.6-beta.16`
	version: String,
	// dunno
	category_id: u64,
	// probably could be an enum at some point, idk
	category_name: Option<String>,
	// idk
	is_primary: bool,
	// size in kb. Same as size_kb.
	size: u64,
	// yup, just the name of the file itself
	file_name: String,
	// yup
	uploaded_timestamp: u64,
	// ISO 8601 with time + ms + offset hr:min? i think?
	uploaded_time: String,
	// uh. Not sure how this is different from `version`
	mod_version: String,
	// Link to virustotal
	external_virus_scan_url: String,
	// Kinda html but also not? has `<br />` and `\n` and `[B]`. Wiki format or smth?
	description: String,
	// kibibytes (multiples of 1024), not kilobytes (multiples of 1000), if that's relevant.
	// using a u32 allows for files up to 4TB. I'm guessing that'll be fine for a good bit of time,
	// seeing how most mods are still only a few mb.
	size_kb: u32,
	// yeah
	size_in_bytes: u64,
	// yeah
	changelog_html: Option<String>,
	// A link to an endpoint which returns a json representation of the list of files which exist
	// in this .zip, formatted like a file/folder structure.
	content_preview_link: String
}

// Ok, we're gonna:
// 1. Login through chrome and let them browse nexus mods on it
// 2. Add an event listener to page load
// 3. On each page load, if the url matches `/stardewvalley/mods/{id}`, look for li#action-nmm and
//    li#action-manual
// 4. Delete the #action-nmm button
// 5. Remove all event listeners from li#action-manual and instead add one that just downloads the
//    mod and `alert("Download queued! Check the Prismatic app for progress.");

// To download:
// 1. Get dependencies.
// 2. Uhhhh do we even need pubgrub? Can we just recurse into each one?
// to download url, need `curl -v -b "nexusmods_session=a40c8fd13148b1517873fa60bdc22d12" -X POST -d 'fid=136001&game_id=1303' 'https://www.nexusmods.com/Core/Libs/Common/Managers/Downloads?GenerateDownloadURL'`
// nexusmods_session cookie is the crux

pub async fn launch_browser(sender: &Sender<BrowserMessage>) -> chromiumoxide::error::Result<()> {
	let mut config = BrowserConfig::builder()
		.with_head()
		.arg("--enable-features=Vulkan,VulkanFromANGLE,DefaultANGLEVulkan")
		.arg("--enable-unsafe-webgpu");

	if std::env::var("WAYLAND_DISPLAY").is_ok() {
		config = config.arg("--ozone-platform=wayland");
	}

	let (browser, mut handle) = Browser::launch(config.build().unwrap()).await?;

	let handle = tokio::spawn(async move {
		// while let Some(res) = handle.next().await {
		loop {
			let next = handle.next().await;
			println!("handle returned result: {next:#?}");
		}
	});

	println!("got browser: {browser:#?}");

	// to sign in, then redirect to stardew valley page
	let new_page = browser.new_page(CreateTargetParams {
		url: "https://users.nexusmods.com/auth/sign_in?redirect_url=https%3A%2F%2Fwww.nexusmods.com%2Fgames%2Fstardewvalley".to_string(),
		// for_tab causes a panic if it's Some(true),
		for_tab: None,
		..CreateTargetParams::default()
	}).await?;

	new_page.set_user_agent(
		"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/96.0.4664.110 Safari/537.36".to_string()
	).await.unwrap();

	println!("new_page: {new_page:#?}");

	for page in browser.pages().await? {
		if page.target_id() != new_page.target_id() {
			println!("closing page {page:#?}");
			page.close().await?;
		}
	}

	let window_id = new_page
		.command_future(GetWindowForTargetParams {
			target_id: Some(new_page.target_id().clone())
		})
		.unwrap()
		.await
		.unwrap()
		.window_id;

	new_page
		.command_future(SetWindowBoundsParams {
			window_id,
			bounds: Bounds {
				left: Some(0),
				top: Some(0),
				width: Some(800),
				height: Some(400),
				window_state: Some(WindowState::Normal)
			}
		})
		.unwrap()
		.await
		.unwrap();

	let mut ev_stream = browser.event_listener::<EventResponseReceived>().await?;

	println!("ev_stream: {ev_stream:#?}");

	while let Some(params) = ev_stream.next().await {
		println!("got response: {:#?}", params.response);

		let serde_json::Value::Object(map) = params.response.headers.inner() else {
			continue;
		};

		let Some(Value::Array(cookies)) = map.get("Set-Cookie") else {
			continue;
		};

		for item in cookies {
			let Value::String(cookie) = item else {
				continue;
			};

			let mut parts = cookie.split('=');

			let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
				continue;
			};

			if key == "nexusmods_session" {
				println!("sending value {value:?}");
				sender
					.send(BrowserMessage::GotSessionCookie(value.to_string()))
					.unwrap();
			}
		}
	}

	println!("returning!");

	handle.await.unwrap();
	drop((browser, new_page));
	Ok(())
}

pub enum BrowserMessage {
	// just the cookie value - the key is `nexusmods_session`
	GotSessionCookie(String),
	Error(String)
}

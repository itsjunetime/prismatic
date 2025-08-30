use std::{
	collections::{BTreeMap, BTreeSet, btree_map::Entry},
	io::Cursor,
	path::Path,
	str::FromStr,
	sync::OnceLock
};

use serde::de::DeserializeOwned;
use ureq::{Agent, Cookie, http};
use zip::{ZipArchive, read::root_dir_common_filter, result::ZipError};

use crate::mod_group::{OverwriteErrStep, UniqueId, overwrite_dir_with_other};

struct Client {
	// get from https://next.nexusmods.com/settings/api-keys
	api_key: String,
	agent: Agent
}

const SDV_DOMAIN_NAME: &str = "stardewvalley";

#[derive(serde::Deserialize)]
struct ModFilesResponse {
	files: Vec<ModFile> // file_updates: Vec<FileUpdate>
}

#[derive(serde::Deserialize)]
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

const NEXUS_COOKIE_URI: &str = "https://nexusmods.com";
const NEXUS_GRAPHQL: &str = "https://api-router.nexusmods.com/graphql";

fn agent(session_cookie: Cookie<'static>) -> &'static ureq::Agent {
	static AGENT: OnceLock<Agent> = OnceLock::new();

	AGENT.get_or_init(|| {
		let agent = ureq::agent();

		let uri = http::uri::Uri::from_static(NEXUS_COOKIE_URI);
		let mut jar = agent.cookie_jar_lock();
		// this shouldn't fail... shouldn't...
		_ = jar.insert(session_cookie, &uri);
		jar.release();

		agent
	})
}

#[derive(serde::Serialize)]
struct GraphqlReq {
	operation_name: &'static str,
	query: &'static str
}

#[derive(serde::Deserialize)]
struct ApiKeyRespBody {
	data: ApiKeyRespBodyInner
}

#[derive(serde::Deserialize)]
struct ApiKeyRespBodyInner {
	personal_api_key: PersonalApiKey
}

#[derive(serde::Deserialize)]
struct PersonalApiKey {
	key: String
}

fn get_apikey(agent: &'static Agent) -> Result<String, ureq::Error> {
	agent
		.post(NEXUS_GRAPHQL)
		.send_json(GraphqlReq {
			operation_name: "PersonalApiKey",
			query: "query PersonalApiKey { personalApiKey { key }} "
		})
		.and_then(|mut resp| resp.body_mut().read_json::<ApiKeyRespBody>())
		.map(|resp| resp.data.personal_api_key.key)
}

const NEXUS_API_ROOT: &str = "https://api.nexusmods.com";

enum ModDownloadErr {
	Ureq(ureq::Error),
	Unzip(ZipError),
	DirCreation {
		step: OverwriteErrStep,
		err: std::io::Error
	}
}

impl From<ureq::Error> for ModDownloadErr {
	fn from(value: ureq::Error) -> Self {
		Self::Ureq(value)
	}
}

impl From<ZipError> for ModDownloadErr {
	fn from(value: ZipError) -> Self {
		Self::Unzip(value)
	}
}

impl From<(OverwriteErrStep, std::io::Error)> for ModDownloadErr {
	fn from((step, err): (OverwriteErrStep, std::io::Error)) -> Self {
		Self::DirCreation { step, err }
	}
}

type SiteModId = u32;

impl Client {
	fn get_all_mod_files(&self, mod_id: &UniqueId) -> Result<ModFilesResponse, ureq::Error> {
		self.get(format!(
			"{NEXUS_API_ROOT}/v1/games/{SDV_DOMAIN_NAME}/mods/{mod_id}/files.json"
		))
	}

	fn get<T: DeserializeOwned>(&self, url: String) -> Result<T, ureq::Error> {
		self.agent
			.get(url)
			.header("apikey", &self.api_key)
			.call()
			.and_then(|resp| resp.into_body().read_json::<T>())
	}

	fn completely_download_mod(
		&self,
		mod_site_id: SiteModId,
		unique_id: &UniqueId,
		mods_dir: &Path
	) -> Result<(), ModDownloadErr> {
		let download_link = self
			.agent
			.post(
				"https://www.nexusmods.com/Core/Libs/Common/Managers/Downloads?GenerateDownloadURL"
			)
			.send(format!("fid={mod_site_id}&game_id=1303"))
			.and_then(|resp| resp.into_body().read_to_string())?;

		let data = self
			.agent
			.get(download_link)
			.call()?
			.into_body()
			.read_to_vec()?;

		let cursor = Cursor::new(data);

		let tmp_dir = mods_dir.join(format!(".tmp.{mod_site_id}"));

		let mut archive = ZipArchive::new(cursor)?;
		archive.extract_unwrapped_root_dir(&tmp_dir, root_dir_common_filter)?;

		let end_dir = mods_dir.join(&*unique_id.0);

		overwrite_dir_with_other(&tmp_dir, &end_dir).map_err(ModDownloadErr::from)
	}

	fn collect_deps_into(
		&self,
		mod_id: SiteModId,
		deps: &mut BTreeMap<SiteModId, BTreeSet<SiteModId>>
	) -> Result<(), ureq::Error> {
		let mods_popup_html = self.agent.get(format!("https://www.nexusmods.com/Core/Libs/Common/Widgets/ModRequirementsPopUp?id={mod_id}&game_id=1303"))
			.call()?
			.into_body()
			.read_to_string()?;

		const LINK_PARENT: &str = "https://www.nexusmods.com/stardewvalley/mods/";

		// get the links that look like https://www.nexusmods.com/stardewvalley/mods/<mod_id>
		// if there are none, then there are no dependencies.

		let mut remaining = mods_popup_html.as_str();
		while let Some(start_idx) = remaining.find(LINK_PARENT) {
			let idx_after = start_idx + LINK_PARENT.len();
			remaining = &remaining[idx_after..];

			let char_count = remaining.chars().take_while(|c| c.is_numeric()).count();
			let dep_mod_id = SiteModId::from_str(&remaining[..char_count]).unwrap();
			match deps.entry(mod_id) {
				Entry::Vacant(entry) => _ = entry.insert(BTreeSet::from_iter([dep_mod_id])),
				Entry::Occupied(mut entry) => _ = entry.get_mut().insert(dep_mod_id)
			}

			if !deps.contains_key(&dep_mod_id) {
				self.collect_deps_into(dep_mod_id, deps)?
			}
		}

		Ok(())
	}
}

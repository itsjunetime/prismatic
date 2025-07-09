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

// to download url, need `curl -v -b "nexusmods_session=a40c8fd13148b1517873fa60bdc22d12" -X POST -d 'fid=136001&game_id=1303' 'https://www.nexusmods.com/Core/Libs/Common/Managers/Downloads?GenerateDownloadURL'`
// nexusmods_session cookie is the crux

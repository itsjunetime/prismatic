# Prismatic

A ***VERY*** work-in-progress Mod manager for Stardew Valley.

## Features:
- [x] Detect mods currently on computer
- [x] Create modgroups as a group of mods
- [x] Run a modgroup using SMAPI

## TODOs
- [ ] Download mods from NexusMods
- [ ] Automatic dependency detection and management within groups
- [ ] Customization of Stardew args/wrapper
- [ ] Updating/Downgrading of Mods
- [ ] Selection of which installed version of Stardew to run (e.g. Steam vs GOG vs wherever else)
- [ ] Nice-to-look-at UI

## Why use this instead of *Other Stardew Mod Manager*?
1. All others that I looked at didn't support arm/aarch64 linux
2. I want automatic dependency management (auto-downloads of dependencies like Farm Type Manager and such) - I didn't see this in the other ones
3. Others didn't have a good solution for downloads inside the mod manager - you had to go to the browser, download, then select from the UI in the mod manager. This is a limitation of the NexusMods API, but we can work around it for a much better experience, so we will.
4. I hate figuring out build systems for other languages (namely, C#), and so using other mod managers would just be a massive pain if they aren't already packaged for me. That's not an issue with rust - just `cargo install --git https://github.com/itsjunetime/prismatic.git` and boom.

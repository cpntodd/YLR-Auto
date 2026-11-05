# Security

Report a vulnerability through
[GitHub's private reporting](https://github.com/cpntodd/YLR-Auto/security/advisories/new)
rather than a public issue. Only the latest release is supported.

## What YLR-Auto touches

- It sends disc-label search terms and your API key to TMDb, and downloads posters
  from TMDb's image server without the key.
- It stores your TMDb key in `~/.config/littlerippa-autosort/config.json`, readable by
  you only.
- It moves and, when you file a duplicate by hand, deletes video files in the folders
  you configure.
- It reads only the destination settings from MakeMKV's `settings.conf`, never the
  licence key.
- It writes desktop entries to your application menu and login startup folders.

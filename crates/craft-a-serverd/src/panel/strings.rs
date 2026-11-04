//! Every user-visible string of the panel, in one place so a translation only
//! has to touch this file (and the templates' static text, which should move
//! here as screens grow). English only for now.

pub const APP_NAME: &str = "craft-a-server";
pub const DISCLAIMER: &str =
    "Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.";

// Pair page
pub const PAIR_TITLE: &str = "Pair this phone";
pub const PAIR_INTRO: &str = "Run craft-a-server pair on the mini PC and type the code it shows.";
pub const PAIR_CODE_LABEL: &str = "Pairing code";
pub const PAIR_DEVICE_LABEL: &str = "Name this phone (optional)";
pub const PAIR_SUBMIT: &str = "Pair";
pub const PAIR_LAN_NOTE: &str = "This panel is only reachable on your home network.";

// Home / devices
pub const HOME_TITLE: &str = "Home";
pub const HOME_PAIRED_AS: &str = "Paired as";
pub const LOGOUT: &str = "Sign out";
pub const DEVICES_TITLE: &str = "Paired phones";
pub const DEVICES_THIS: &str = "this phone";
pub const DEVICES_REVOKE: &str = "Sign out";
pub const DEVICES_EMPTY: &str = "No phones are paired.";
pub const NAV_HOME: &str = "Home";

// Errors (page and JSON)
pub const ERR_PAGE_TITLE: &str = "Something went wrong";
pub const ERR_BACK: &str = "Back to the panel";
pub const ERR_PAIR_INVALID: &str = "That code is not valid or has expired";
pub const ERR_UNAUTHORIZED: &str = "Please pair this phone first";
pub const ERR_FORBIDDEN: &str = "That request was not allowed";
pub const ERR_CSRF: &str = "Your session check failed. Reload the page and try again";
pub const ERR_ORIGIN: &str = "That request did not come from this panel";
pub const ERR_HOST: &str = "This address is not accepted by the panel";
pub const ERR_NOT_FOUND: &str = "Nothing here";
pub const ERR_METHOD: &str = "That action is not supported here";
pub const ERR_TOO_LARGE: &str = "That request is too large";
pub const ERR_BAD_REQUEST: &str = "That request could not be understood";
pub const ERR_TIMEOUT: &str = "The request took too long";
pub const ERR_BUSY: &str = "The panel is busy. Try again in a moment";
pub const ERR_INTERNAL: &str =
    "Something went wrong inside the panel. Details are in the daemon log";
pub const ERR_RATE_LIMITED: &str = "Too many attempts";

// Device names
pub const DEVICE_ANDROID: &str = "Android phone";
pub const DEVICE_ANDROID_TABLET: &str = "Android tablet";
pub const DEVICE_IPHONE: &str = "iPhone";
pub const DEVICE_IPAD: &str = "iPad";
pub const DEVICE_BROWSER: &str = "Browser";

// Navigation
pub const NAV_BACKUPS: &str = "Backups";
pub const NAV_PLAYERS: &str = "Players";
pub const NAV_LOGS: &str = "Logs";
pub const NAV_MODS: &str = "Mods";
pub const NAV_ONLINE: &str = "Online";
pub const NAV_SETTINGS: &str = "Settings";

// Server card
pub const CARD_NO_SERVER: &str = "You have no server yet.";
pub const CARD_CREATE_LINK: &str = "Create your server";
pub const CARD_VERSION: &str = "Minecraft";
pub const CARD_PLAYERS: &str = "Players online";
pub const CARD_ADDRESS_LOCAL: &str = "Join at home";
pub const CARD_ADDRESS_IP: &str = "If that name does not work";
pub const CARD_ADDRESS_ONLINE: &str = "Friends outside your home";
pub const CARD_START: &str = "Start";
pub const CARD_STOP: &str = "Stop";
pub const CARD_RESTART: &str = "Restart";
pub const CARD_RESET: &str = "Reset";
pub const CARD_DELETE: &str = "Delete server";
pub const CARD_WHY_FAILED: &str = "What went wrong";
pub const STATE_CREATING: &str = "Creating";
pub const STATE_STOPPED: &str = "Stopped";
pub const STATE_STARTING: &str = "Starting";
pub const STATE_RUNNING: &str = "Running";
pub const STATE_STOPPING: &str = "Stopping";
pub const STATE_FAILED: &str = "Failed";

// Busy and result banners
pub const BUSY_START: &str = "Starting the server. This can take a few minutes.";
pub const BUSY_STOP: &str = "Stopping the server.";
pub const BUSY_RESTART: &str = "Restarting the server. This can take a few minutes.";
pub const BUSY_RESET: &str = "Resetting the server.";
pub const BUSY_DELETE: &str = "Deleting the server.";
pub const BUSY_BACKUP: &str = "Creating a backup.";
pub const BUSY_RESTORE: &str = "Restoring the backup. The server may restart.";
pub const DONE_START: &str = "The server is running.";
pub const DONE_STOP: &str = "The server is stopped.";
pub const DONE_RESTART: &str = "The server restarted.";
pub const DONE_RESET: &str = "The server was reset. You can start it again.";
pub const DONE_DELETE: &str = "The server was deleted.";
pub const DONE_CREATE: &str = "Server created. Press Start when you are ready.";
pub const BUSY_CREATE: &str = "Creating the server.";
pub const DONE_BACKUP: &str = "Backup created.";
pub const DONE_RESTORE: &str = "Backup restored.";
pub const ERR_BUSY_ACTION: &str = "Another action is still running. Wait for it to finish";
pub const ERR_VIEW_DISTANCE: &str = "Choose one of the view distances in the list";
pub const ERR_NO_SERVER: &str = "There is no server yet. Create one first";
pub const ERR_SERVER_EXISTS: &str = "A server already exists. Delete it before creating another";
pub const ERR_ACTION_FAILED: &str = "That did not work. Details are in the daemon log";
pub const ERR_START_FAILED: &str = "The server did not start. See the Logs page for details";
pub const ERR_DISK_LOW: &str = "There is not enough free disk space for that";
pub const ERR_MEMORY: &str = "This machine does not have enough memory to run a server";
pub const ERR_EULA: &str = "You must accept the Minecraft EULA to create a server";
pub const ERR_CONFIRM: &str = "That action needs to be confirmed first";
pub const ERR_NOT_RUNNING: &str = "The server must be running for that";
pub const ERR_SERVER_STATE: &str = "The server is busy or in the wrong state for that";

// Create server
pub const CREATE_TITLE: &str = "Create your server";
pub const CREATE_NAME_LABEL: &str = "Server name";
pub const CREATE_NAME_DEFAULT: &str = "My server";
pub const CREATE_VERSION_LABEL: &str = "Minecraft version";
pub const CREATE_VERSION_DEFAULT: &str = "26.3";
pub const CREATE_VERSION_HELP: &str =
    "Java Edition. Players need to join with this same version. Releases are the safe choice.";
pub const CREATE_VERSION_SNAPSHOTS: &str = "Snapshots and pre-releases";
pub const CREATE_MEMORY: &str = "Memory for the game";
pub const CREATE_MEMORY_LOW: &str =
    "This machine has little memory. Expect the server to struggle with several players.";
pub const CREATE_MEMORY_NONE: &str = "This machine does not have enough memory to run a server.";
pub const CREATE_EULA_LABEL: &str = "I accept the Minecraft EULA";
pub const CREATE_EULA_LINK: &str = "Read the EULA";
pub const CREATE_EULA_URL: &str = "https://www.minecraft.net/en-us/eula";
pub const CREATE_SUBMIT: &str = "Create server";
pub const CREATE_WHITELIST_NOTE: &str = "Only players you add on the Players page can join.";

// Backups
pub const BACKUPS_TITLE: &str = "Backups";
pub const BACKUPS_CREATE: &str = "Back up now";
pub const BACKUPS_EMPTY: &str = "No backups yet.";
pub const BACKUPS_RESTORE: &str = "Restore";
pub const BACKUPS_NOTE: &str =
    "A backup is also made every day. Old ones are removed automatically.";
pub const BACKUP_DAILY: &str = "Daily";
pub const BACKUP_MANUAL: &str = "Manual";
pub const BACKUP_PRE_CHANGE: &str = "Before a change";
pub const BACKUP_PRE_RESTORE: &str = "Before a restore";
pub const RESTORE_TITLE: &str = "Restore this backup?";
pub const RESTORE_WARNING: &str = "The current world is replaced by this backup. A safety backup of the current world is taken first, and the server restarts if it was running.";
pub const RESTORE_CONFIRM: &str = "Yes, restore";
pub const CANCEL: &str = "Cancel";

// Reset / delete confirmation
pub const RESET_TITLE: &str = "Reset the server?";
pub const RESET_WARNING: &str =
    "This clears the failed state so the server can be started again. Your world is not touched.";
pub const RESET_CONFIRM: &str = "Yes, reset";
pub const DELETE_TITLE: &str = "Delete the server?";
pub const DELETE_WARNING: &str = "This permanently deletes the world, the settings and all backups of this server. This cannot be undone.";
pub const DELETE_CONFIRM: &str = "Yes, delete everything";

// Players
pub const PLAYERS_TITLE: &str = "Players";
pub const PLAYERS_WHITELIST: &str = "Allowed players";
pub const PLAYERS_EMPTY: &str = "Nobody is on the list yet.";
pub const PLAYERS_NAME_LABEL: &str = "Player name";
pub const PLAYERS_NAME_HELP: &str =
    "3 to 16 letters, digits or underscores, exactly as in Minecraft.";
pub const PLAYERS_ADD: &str = "Add";
pub const PLAYERS_REMOVE: &str = "Remove";
pub const PLAYERS_KICK: &str = "Kick";
pub const PLAYERS_NEED_RUNNING: &str = "Start the server to manage players.";
pub const PLAYERS_ADDED: &str = "Added to the list";
pub const PLAYERS_REMOVED: &str = "Removed from the list";
pub const PLAYERS_KICKED: &str = "Kick sent";
pub const ERR_PLAYER_NAME: &str = "Player names are 3 to 16 letters, digits or underscores";

// Logs
pub const LOGS_TITLE: &str = "Logs";
pub const LOGS_EMPTY: &str = "No log lines yet.";
pub const LOGS_NOTE: &str = "Newest lines at the bottom. Updates every 2 seconds.";

// Mods
pub const MODS_TITLE: &str = "Mods";
pub const MODS_NOTE: &str = "Mods come from Modrinth. Before every change the server is backed up, and it goes back to how it was if it does not start with the change.";
pub const MODS_SEARCH_LABEL: &str = "Search for a mod";
pub const MODS_SEARCH: &str = "Search";
pub const MODS_INCLUDE_UNKNOWN: &str =
    "Also show mods that do not say whether they work on servers";
pub const MODS_CHECK_UPDATES: &str = "Check for updates";
pub const MODS_RESULTS: &str = "Results";
pub const MODS_NO_RESULTS: &str = "No mods found for this Minecraft version.";
pub const MODS_INSTALL: &str = "Install";
pub const MODS_INSTALLED_TAG: &str = "Installed";
pub const MODS_DEPENDENCY_TAG: &str = "Needed by another mod";
pub const MODS_INSTALLED: &str = "Installed mods";
pub const MODS_NONE: &str = "No mods installed yet.";
pub const MODS_UNTRACKED: &str = "Other files in the mods folder (not managed here)";
pub const MODS_NEEDED_BY: &str = "Needed by";
pub const MODS_REMOVE: &str = "Remove";
pub const MODS_SIDE_REQUIRED: &str = "Server: required";
pub const MODS_SIDE_OPTIONAL: &str = "Server: optional";
pub const MODS_SIDE_UNKNOWN: &str = "Server: not stated";
pub const MODS_STATUS_MISSING: &str = "File missing";
pub const MODS_STATUS_CHANGED: &str = "File changed";
pub const MODS_INSTALL_TITLE: &str = "Install this mod?";
pub const MODS_UPDATE_TITLE: &str = "Update this mod?";
pub const MODS_CANNOT: &str = "This cannot be installed:";
pub const MODS_REPLACES: &str = "Replaces";
pub const MODS_ALSO_INSTALLED: &str = "These required mods will be installed too";
pub const MODS_ALREADY: &str = "Already installed:";
pub const MODS_OPTIONAL: &str = "Optional extras you may want, not installed:";
pub const MODS_RESTART_WARNING: &str = "This will restart the server. Players are disconnected for a minute or more. A backup is taken first, and if the server does not start with the change, everything goes back to how it was.";
pub const MODS_STOPPED_NOTE: &str = "The server is stopped, so the change cannot be tried out. A backup is taken first. It takes effect the next time the server starts.";
pub const MODS_INSTALL_CONFIRM: &str = "Yes, install and restart";
pub const MODS_REMOVE_TITLE: &str = "Remove this mod?";
pub const MODS_DEPENDENTS_WARNING: &str = "Other mods need this one and may stop working:";
pub const MODS_REMOVE_ORPHANS: &str = "Also remove what only this mod needed:";
pub const MODS_REMOVE_CONFIRM: &str = "Yes, remove and restart";
pub const MODS_UPDATES_TITLE: &str = "Mod updates";
pub const MODS_UP_TO_DATE: &str = "All mods are up to date.";
pub const MODS_UPDATE: &str = "Update";
pub const MODS_CATEGORY_ALL: &str = "All";
pub const MODS_SORT_AUTO: &str = "Best match";
pub const MODS_SORT_DOWNLOADS: &str = "Most downloaded";
pub const MODS_SORT_FOLLOWS: &str = "Most followed";
pub const MODS_SORT_NEWEST: &str = "Newest";
pub const MODS_SORT_UPDATED: &str = "Recently updated";
pub const MODS_BROWSE_TITLE: &str = "Find mods";
pub const MODS_PREV: &str = "Previous";
pub const MODS_NEXT: &str = "Next";
pub const BUSY_MODS_INSTALL: &str =
    "Installing the mod. The server restarts, which can take a few minutes.";
pub const BUSY_MODS_REMOVE: &str =
    "Removing the mod. The server restarts, which can take a few minutes.";
pub const DONE_MODS_INSTALL: &str = "The mod was installed.";
pub const DONE_MODS_REMOVE: &str = "The mod was removed.";

// Settings
pub const SETTINGS_TITLE: &str = "Settings";
pub const SETTINGS_ADDRESS: &str = "Panel address";
pub const SETTINGS_ADDRESS_HELP: &str = "Some Android phones cannot open .local names. Use the number address below, or scan the code with another phone.";
pub const SETTINGS_ADDRESS_NAME: &str = "By name";
pub const SETTINGS_ADDRESS_IP: &str = "By number";
pub const SETTINGS_NO_IP: &str = "The number address could not be determined on this machine.";
pub const SETTINGS_QR_LABEL: &str = "QR code of the panel address";
pub const SETTINGS_SERVER_ADDRESS: &str = "Minecraft address";
pub const SETTINGS_DEVICES: &str = "Paired phones";

// Online play (playit.gg tunnel)
pub const ONLINE_TITLE: &str = "Online play";
pub const ONLINE_INTRO: &str = "Let friends who are not on your home network join your server. It is off by default and works without changing your router.";
pub const ONLINE_STATE: &str = "Status";
pub const ONLINE_STATE_OFF: &str = "Off";
pub const ONLINE_STATE_PREPARING: &str = "Getting ready";
pub const ONLINE_STATE_CLAIMING: &str = "Waiting for you to link playit.gg";
pub const ONLINE_STATE_CLAIMED: &str = "Linked, waiting for a tunnel";
pub const ONLINE_STATE_RUNNING: &str = "On";
pub const ONLINE_STATE_ERROR: &str = "Problem";
pub const ONLINE_PREPARING_HELP: &str =
    "Downloading the playit agent and checking it against a fixed checksum. This takes a moment.";
pub const ONLINE_CLAIM_HELP: &str = "Open this link on any device, sign in to playit.gg (a guest account works) and accept. This page updates by itself.";
pub const ONLINE_CLAIM_LINK: &str = "Link to open";
pub const ONLINE_CLAIM_CODE: &str = "Code";
pub const ONLINE_CLAIM_OPEN: &str = "Open the playit.gg link";
pub const ONLINE_CLAIMED_HELP: &str = "This machine is linked to playit.gg. In the playit.gg dashboard, create a Minecraft Java tunnel that forwards to";
pub const ONLINE_ADDRESS: &str = "Friends can join at";
pub const ONLINE_ADDRESS_HELP: &str = "Only people on your whitelist can join. Players on your home network keep using the home address.";
pub const ONLINE_ERROR_RETRY: &str = "It is retried automatically.";
pub const ONLINE_OFF_KEPT: &str =
    "The link to playit.gg is kept, so turning it on again needs no new sign-in.";
pub const ONLINE_ENABLE: &str = "Turn on online play";
pub const ONLINE_DISABLE: &str = "Turn off";
pub const ONLINE_UNLINK: &str = "Turn off and unlink playit.gg";
pub const ONLINE_UNAVAILABLE: &str = "Online play is not available in this daemon.";
pub const ONLINE_AGENT: &str = "playit agent";
pub const ONLINE_ENABLE_TITLE: &str = "Turn on online play?";
pub const ONLINE_ENABLE_LEAD: &str = "Before you turn this on, here is what it means:";
pub const ONLINE_ENABLE_CONFIRM: &str = "I understand, turn it on";
pub const ONLINE_DISCLOSURE_TRAFFIC: &str = craft_a_server_core::tunnel::DISCLOSURE_TRAFFIC;
pub const ONLINE_DISCLOSURE_IPS: &str = craft_a_server_core::tunnel::DISCLOSURE_IPS;
pub const ONLINE_DISCLOSURE_POLICY: &str = craft_a_server_core::tunnel::DISCLOSURE_POLICY;
pub const ONLINE_DISCLOSURE_ACCOUNT: &str = craft_a_server_core::tunnel::DISCLOSURE_ACCOUNT;
pub const ONLINE_DISCLOSURE_OFF: &str = craft_a_server_core::tunnel::DISCLOSURE_OFF;

pub const NAV_PERFORMANCE: &str = "Performance";
pub const PERF_TITLE: &str = "Performance";
pub const PERF_INTRO: &str = "See how much of this machine the server uses, and choose how far players can see. A smaller view distance is the easiest way to make a busy server run lighter.";
pub const PERF_USAGE: &str = "Right now";
pub const PERF_NOT_RUNNING: &str =
    "The server is not running, so there is nothing to measure yet. Start it from Home.";
pub const PERF_NO_READING: &str = "No reading from the game yet. This page updates by itself.";
pub const PERF_GAME: &str = "The game";
pub const PERF_GAME_MEMORY: &str = "Memory in use";
pub const PERF_GAME_HEAP: &str = "Memory given to Java";
pub const PERF_GAME_CPU: &str = "Processor";
pub const PERF_GAME_PROCESSES: &str = "Processes";
pub const PERF_MEASURING: &str = "Measuring...";
pub const PERF_MACHINE: &str = "This machine";
pub const PERF_MACHINE_MEMORY: &str = "Memory in use";
pub const PERF_MACHINE_LOAD: &str = "Load (last minute)";
pub const PERF_MACHINE_DISK: &str = "Free disk space";
pub const PERF_WARN_MEMORY: &str =
    "This machine is almost out of memory. Lower the view distance or stop other programs.";
pub const PERF_WARN_DISK: &str =
    "Free disk space is low. Delete old backups or the server may stop saving.";
pub const PERF_VIEW_TITLE: &str = "View distance";
pub const PERF_VIEW_HELP: &str = "How many chunks around each player the server sends. Higher looks better but costs a lot of memory and processor time; the number goes up with the square of the distance.";
pub const PERF_VIEW_LABEL: &str = "Chunks";
pub const PERF_VIEW_DEFAULT: &str = "game default";
pub const PERF_VIEW_SAVE: &str = "Save";
pub const PERF_VIEW_APPLIES: &str = "A change takes effect the next time the server starts. Restart it from Home when nobody is playing.";
pub const DONE_VIEW_DISTANCE: &str = "View distance saved";

// World import
pub const NAV_WORLD: &str = "World";
pub const WORLD_TITLE: &str = "Import a world";
pub const WORLD_NOTE: &str = "Upload a .zip of a Minecraft world from this computer. It can be just the world folder or a whole server folder.";
pub const WORLD_BRINGS: &str = "What it can bring";
pub const WORLD_BRINGS_ITEMS: [&str; 5] = [
    "The world itself: terrain, players, advancements and datapacks",
    "Mods (Fabric mods only) and their settings",
    "The whitelist, operators and banned players",
    "The server icon (a 64x64 PNG image)",
    "Gameplay settings such as difficulty and game mode",
];
pub const WORLD_NEVER: &str = "The server program, libraries, logs, scripts and the EULA file are never imported. Online mode and the whitelist always stay on.";
pub const WORLD_FILE_LABEL: &str = "Zip file";
pub const WORLD_LIMIT: &str = "Largest file that fits right now";
pub const WORLD_UPLOAD: &str = "Upload and check";
pub const WORLD_NEEDS_JS: &str = "Uploading needs JavaScript, which is turned off in this browser.";
pub const WORLD_PENDING: &str = "A file is already waiting to be imported.";
pub const WORLD_PENDING_REVIEW: &str = "Review it";
pub const WORLD_REVIEW_TITLE: &str = "Review the import";
pub const WORLD_REVIEW_WARNING: &str = "This replaces the current world with the one in the file. A backup is taken first and the server restarts. If the server cannot start with the new files, the old ones are put back by themselves.";
pub const WORLD_REVIEW_WORLD: &str = "World";
pub const WORLD_REVIEW_MODS: &str = "Mods";
pub const WORLD_REVIEW_MODS_REPLACE: &str =
    "All current mods are replaced by these. Mods that are not for Fabric are skipped.";
pub const WORLD_REVIEW_MODS_NONE: &str = "No mods in the file: the current mods stay.";
pub const WORLD_REVIEW_MODS_EMPTY: &str =
    "The file has an empty mods folder: all current mods are removed.";
pub const WORLD_REVIEW_CONFIG: &str = "Mod settings";
pub const WORLD_REVIEW_LISTS: &str = "Player lists";
pub const WORLD_REVIEW_ICON: &str = "Server icon";
pub const WORLD_REVIEW_SETTINGS: &str = "Gameplay settings";
pub const WORLD_REVIEW_IGNORED: &str = "Files that are not used";
pub const WORLD_REVIEW_YES: &str = "Yes";
pub const WORLD_CONFIRM: &str = "Replace the world and import";
pub const WORLD_DISCARD: &str = "Discard the upload";
pub const BUSY_WORLD_UPLOAD: &str = "Receiving the file. Keep this page open until it finishes.";
pub const BUSY_WORLD_IMPORT: &str =
    "Importing the world. The server restarts, which can take a few minutes.";
pub const DONE_WORLD_UPLOAD: &str = "File received.";
pub const DONE_WORLD_DISCARD: &str = "The uploaded file was deleted.";
pub const ERR_NO_UPLOAD: &str = "There is no uploaded file waiting. Upload one first";
pub const ERR_UPLOAD_LENGTH: &str = "The upload did not say how big it is";
pub const ERR_UPLOAD_NOT_ZIP: &str = "That file is not a zip archive";
pub const ERR_UPLOAD_INCOMPLETE: &str = "The upload did not finish. Try again";

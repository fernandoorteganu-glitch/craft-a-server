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
pub const HOME_PLACEHOLDER: &str = "Your servers will appear here soon.";
pub const LOGOUT: &str = "Sign out";
pub const DEVICES_TITLE: &str = "Paired phones";
pub const DEVICES_THIS: &str = "this phone";
pub const DEVICES_REVOKE: &str = "Sign out";
pub const DEVICES_EMPTY: &str = "No phones are paired.";
pub const NAV_HOME: &str = "Home";
pub const NAV_DEVICES: &str = "Phones";

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

//! Shared canonical-tag constants used by `#[cfg(test)]` modules in
//! `digest_cache`, `intent::dict`, and `wechat::router`.
//!
//! These three test suites all need to agree on what string a "Dubai"
//! / "Turkey" / "art" canonical tag actually is — because the whole
//! routing pipeline depends on the skill's data, the dict's tags, and
//! the router's assertions all using IDENTICAL byte sequences. Keeping
//! one source of truth here prevents drift (e.g. if someone renames
//! `"AbuDhabi"` → `"Abu_Dhabi"` in the dict test but forgets the
//! digest test, the tests pass individually but the production
//! pipeline mismatches).
//!
//! The module is gated `#[cfg(test)]` so it's stripped from the
//! release binary — these are test-only constants, not production
//! defaults.

#![cfg(test)]

// ----- Countries -----
pub(crate) const COUNTRY_UAE: &str = "UAE";
pub(crate) const COUNTRY_TURKEY: &str = "Turkey";
pub(crate) const COUNTRY_NEPAL: &str = "Nepal";

// ----- Cities -----
pub(crate) const CITY_DUBAI: &str = "Dubai";
pub(crate) const CITY_ABU_DHABI: &str = "AbuDhabi";
pub(crate) const CITY_SHARJAH: &str = "Sharjah";
pub(crate) const CITY_ISTANBUL: &str = "Istanbul";
pub(crate) const CITY_KATHMANDU: &str = "Kathmandu";

/// A city tag intentionally NOT present in any fixture dict. Use in
/// "no match" / "miss" path tests.
pub(crate) const CITY_NO_MATCH: &str = "Mars";

// ----- Categories -----
pub(crate) const CATEGORY_ART: &str = "art";
pub(crate) const CATEGORY_MUSIC: &str = "music";

// ----- Date / time tags -----
pub(crate) const TAG_TODAY: &str = "today";
pub(crate) const TAG_WEEKEND: &str = "weekend";
pub(crate) const TIME_EVENING: &str = "evening";

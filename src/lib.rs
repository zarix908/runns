//! Who may enter a named network namespace without being root.
//!
//! netns-singbox writes the allowed uid while the namespace exists. runns reads
//! it before `setns`. See [`owner_stamp_path`].

mod owner_stamp;

pub use owner_stamp::{is_named_netns_path, netns_name_ok, owner_stamp_path, parse_owner_stamp};

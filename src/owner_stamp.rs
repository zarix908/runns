//! netns-singbox writes `/run/netns-singbox/<name>.user` (uid) while the
//! namespace exists. The namespace itself stays owned by the initial user
//! namespace, so the entered process cannot administer nftables or routes.

const OWNER_DIR: &str = "/run/netns-singbox";

pub fn owner_stamp_path(netns_name: &str) -> std::path::PathBuf {
    std::path::Path::new(OWNER_DIR).join(format!("{netns_name}.user"))
}

pub fn parse_owner_stamp(text: &str) -> Option<u32> {
    text.split_whitespace().next()?.parse().ok()
}

pub fn netns_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && !name.contains("..")
}

/// Unprivileged `runns` may only join the named inode `ip netns` publishes.
pub fn is_named_netns_path(name: &str, path: &std::path::Path) -> bool {
    netns_name_ok(name)
        && (path == std::path::Path::new("/run/netns").join(name)
            || path == std::path::Path::new("/var/run/netns").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_stamp_parses_uid() {
        assert_eq!(parse_owner_stamp("1000\n"), Some(1000));
        assert_eq!(parse_owner_stamp("  1000 extra\n"), Some(1000));
        assert_eq!(parse_owner_stamp(""), None);
        assert_eq!(parse_owner_stamp("nope\n"), None);
    }

    #[test]
    fn named_netns_path_rejects_foreign_inode_paths() {
        assert!(is_named_netns_path(
            "agent",
            std::path::Path::new("/run/netns/agent")
        ));
        assert!(is_named_netns_path(
            "agent",
            std::path::Path::new("/var/run/netns/agent")
        ));
        assert!(!is_named_netns_path(
            "net",
            std::path::Path::new("/proc/1/ns/net")
        ));
        assert!(!is_named_netns_path(
            "../agent",
            std::path::Path::new("/run/netns/agent")
        ));
    }

    #[test]
    fn netns_name_is_one_segment() {
        assert!(netns_name_ok("agent"));
        assert!(!netns_name_ok("a/b"));
        assert!(!netns_name_ok(".."));
        assert!(!netns_name_ok(""));
    }
}

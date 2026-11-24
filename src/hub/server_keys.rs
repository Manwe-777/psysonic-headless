use sha2::{Digest, Sha256};
use url::Url;

/// The address as Psysonic normalises it before hashing: scheme, lower-case host (with a non-default
/// port), and the path without trailing slashes.
fn normalize_url(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(u) => {
            let host = u.host_str().unwrap_or_default().to_lowercase();
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}://{host}{port}{}", u.scheme(), u.path().trim_end_matches('/'))
        }
        Err(_) => raw.trim().to_lowercase().trim_end_matches('/').to_string(),
    }
}

/// The opaque "music server + account" identifier Psysonic sends to the hub, so playback is only
/// handed between devices that would load the same queue. Only this hash leaves the device.
/// Subsonic servers match user names without regard to case, so "Name" and "name" are one account.
pub fn server_key(server_url: &str, user: &str) -> String {
    address_hash(server_url, &user.trim().to_lowercase())
}

/// A hash of an address and a name, the name kept as typed. Used where only a stable identifier is
/// needed (the device id, the cache folder), so those stay the same as before the hub key ignored case.
pub fn address_hash(url: &str, name: &str) -> String {
    let digest = Sha256::digest(format!("{}\n{}", normalize_url(url), name.trim()));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values computed with Psysonic's own normalisation (hubServerKeys.ts).
    #[test]
    fn matches_psysonic_so_handoffs_between_them_are_allowed() {
        assert_eq!(
            server_key("https://Music.Example.com/navidrome/", "me"),
            "389b6e1262d0164d0fd736e2c69cb8e321c2e9cbcb845c16c41b5d3647893a8e"
        );
        assert_eq!(
            server_key("http://192.168.1.5:4533", " Alice "),
            "f43b2a69e05f45ad9c8a861997bc07891a7191f85cc4cf520480470e77ee7dda"
        );
        assert_eq!(
            server_key("https://music.example.com:443/", "me"),
            "6245333ba74c9c8c905086803fc9a379638ff28a97ad1f80a91f0dd0c5431eb2"
        );
    }
    #[test]
    fn a_user_name_in_other_capitals_is_the_same_account() {
        assert_eq!(
            server_key("https://music.example.com", "Manuh"),
            server_key("https://music.example.com", "manuh")
        );
    }

    // The device id and the cache folder come from this hash, so it must not change for existing
    // players (a new device id makes the hub see a new device).
    #[test]
    fn the_address_hash_keeps_the_name_as_typed() {
        assert_eq!(
            address_hash("http://192.168.1.5:4533", " Alice "),
            "62a1d888a2d484aa9a4a4e31a0668b88e155c52172ad6dd551f7a5b4db0dd73c"
        );
    }
}

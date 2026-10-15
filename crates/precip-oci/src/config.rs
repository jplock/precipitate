//! The OCI CLI's configuration file, an INI file: `~/.oci/config`, or
//! `OCI_CLI_CONFIG_FILE`. Each profile is an INI section; every profile
//! inherits the keys of `[DEFAULT]`.

use crate::sign::{KeyError, Signer, parse_key};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use camino::{Utf8Path, Utf8PathBuf};
use configparser::ini::Ini;

const DEFAULT: &str = "DEFAULT";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(Utf8PathBuf, String),
    #[error("no profile [{profile}] in {path}")]
    NoProfile { profile: String, path: Utf8PathBuf },
    #[error("profile [{profile}] sets no {field}")]
    Missing { profile: String, field: &'static str },
    #[error("loading the key {0}")]
    Key(Utf8PathBuf, #[source] KeyError),
    #[error("the tenancy {0} is not a tenancy OCID")]
    Tenancy(String),
}

/// One profile's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    pub tenancy: String,
    pub user: Option<String>,
    pub fingerprint: Option<String>,
    pub key_file: Utf8PathBuf,
    pub region: String,
    pub security_token_file: Option<Utf8PathBuf>,
}

/// `~` at the start of a path is the home directory, as the CLI writes
/// it.
fn expand(path: &str) -> Utf8PathBuf {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => Utf8Path::new(&home).join(rest),
        (Some(_) | None, _) => Utf8PathBuf::from(path),
    }
}

pub(crate) fn parse(
    text: &str,
    path: &Utf8Path,
    profile: &str,
) -> Result<Profile, ConfigError> {
    // Section and key names keep their case, as the OCI CLI reads them,
    // and every profile falls back to [DEFAULT].
    let mut defaults = Ini::new_cs().defaults();
    defaults.default_section = DEFAULT.into();
    defaults.cascade_defaults = true;
    let mut ini = Ini::new_from_defaults(defaults);
    ini.read(text.to_string())
        .map_err(|why| ConfigError::Parse(path.into(), why))?;
    if !ini.sections().iter().any(|s| s == profile) {
        return Err(ConfigError::NoProfile {
            profile: profile.into(),
            path: path.into(),
        });
    }
    let get = |k: &str| ini.get(profile, k).filter(|v| !v.is_empty());
    let need = |k: &'static str| {
        get(k).ok_or(ConfigError::Missing { profile: profile.into(), field: k })
    };
    let tenancy = need("tenancy")?;
    if !tenancy.starts_with("ocid1.tenancy.") {
        return Err(ConfigError::Tenancy(tenancy));
    }
    Ok(Profile {
        tenancy,
        user: get("user"),
        fingerprint: get("fingerprint"),
        key_file: expand(&need("key_file")?),
        region: need("region")?,
        security_token_file: get("security_token_file").map(|p| expand(&p)),
    })
}

/// The configuration file to read.
pub(crate) fn default_path() -> Utf8PathBuf {
    std::env::var("OCI_CLI_CONFIG_FILE")
        .ok()
        .filter(|v| !v.is_empty())
        .map_or_else(|| expand("~/.oci/config"), Utf8PathBuf::from)
}

/// The profile to use: `profile`, else `OCI_CLI_PROFILE`, else
/// `DEFAULT`.
pub(crate) fn profile_name(profile: Option<&str>) -> String {
    profile
        .map(str::to_string)
        .or_else(|| {
            std::env::var("OCI_CLI_PROFILE").ok().filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| DEFAULT.into())
}

pub(crate) fn load(
    path: &Utf8Path,
    profile: &str,
) -> Result<Profile, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(path.into(), e))?;
    parse(&text, path, profile)
}

/// The realm of a tenancy: `oc1` in `ocid1.tenancy.oc1..`.
pub(crate) fn realm(tenancy: &str) -> Result<String, ConfigError> {
    tenancy
        .strip_prefix("ocid1.tenancy.")
        .and_then(|r| r.split('.').next())
        .filter(|r| r.starts_with("oc"))
        .map(str::to_string)
        .ok_or_else(|| ConfigError::Tenancy(tenancy.into()))
}

/// The domain a tenancy's realm puts its service endpoints under.
pub(crate) fn domain(tenancy: &str) -> Result<String, ConfigError> {
    let realm = realm(tenancy)?;
    Ok(match realm.as_str() {
        "oc1" => "oraclecloud.com".into(),
        "oc2" | "oc3" => "oraclegovcloud.com".into(),
        "oc4" => "oraclegovcloud.uk".into(),
        "oc19" => "oraclecloud.eu".into(),
        other => format!("oraclecloud{}.com", other.trim_start_matches("oc")),
    })
}

/// Who a profile signs in as: the user, or the subject of a session
/// token. The token is not verified; this only labels the scan.
pub(crate) fn caller(profile: &Profile, token: Option<&str>) -> String {
    if let Some(user) = &profile.user {
        return user.clone();
    }
    #[derive(serde::Deserialize)]
    struct Claims {
        sub: String,
    }
    token
        .and_then(|t| t.split('.').nth(1))
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Claims>(&b).ok())
        .map_or_else(|| "unknown".into(), |c| c.sub)
}

/// The signer a profile describes, and who it signs as.
pub(crate) fn signer(
    profile: &Profile,
    name: &str,
) -> Result<(Signer, String), ConfigError> {
    let pem = std::fs::read_to_string(&profile.key_file)
        .map_err(|e| ConfigError::Read(profile.key_file.clone(), e))?;
    let key = parse_key(&pem)
        .map_err(|e| ConfigError::Key(profile.key_file.clone(), e))?;
    let (key_id, token) = match &profile.security_token_file {
        Some(path) => {
            let token = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::Read(path.clone(), e))?;
            let token = token.trim().to_string();
            (format!("ST${token}"), Some(token))
        }
        None => {
            let missing =
                |field| ConfigError::Missing { profile: name.into(), field };
            let user =
                profile.user.as_deref().ok_or_else(|| missing("user"))?;
            let fingerprint = profile
                .fingerprint
                .as_deref()
                .ok_or_else(|| missing("fingerprint"))?;
            (format!("{}/{user}/{fingerprint}", profile.tenancy), None)
        }
    };
    let caller = caller(profile, token.as_deref());
    Ok((Signer::new(key_id, key), caller))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::test_key;

    const TEXT: &str = "
# comment
[DEFAULT]
tenancy = ocid1.tenancy.oc1..aaa
region = us-ashburn-1
key_file = /keys/default.pem
user=ocid1.user.oc1..u
fingerprint=aa:bb

[prod]
region = eu-frankfurt-1

[session]
security_token_file = /tok
user =
";

    #[test]
    fn profiles_inherit_default_and_override_it() {
        let p = parse(TEXT, Utf8Path::new("cfg"), "prod").unwrap();
        assert_eq!(p.region, "eu-frankfurt-1");
        assert_eq!(p.tenancy, "ocid1.tenancy.oc1..aaa");
        assert_eq!(p.key_file, Utf8PathBuf::from("/keys/default.pem"));
        let s = parse(TEXT, Utf8Path::new("cfg"), "session").unwrap();
        assert_eq!(s.user, None, "a blank value is unset");
        assert_eq!(s.security_token_file, Some(Utf8PathBuf::from("/tok")));
        assert!(matches!(
            parse(TEXT, Utf8Path::new("cfg"), "nope"),
            Err(ConfigError::NoProfile { .. })
        ));
        assert!(matches!(
            parse(
                "[DEFAULT]\ntenancy=ocid1.tenancy.oc1..a\n",
                Utf8Path::new("cfg"),
                "DEFAULT"
            ),
            Err(ConfigError::Missing { field: "key_file", .. })
        ));
        assert!(matches!(
            parse("[DEFAULT]\ntenancy=x\n", Utf8Path::new("cfg"), "DEFAULT"),
            Err(ConfigError::Tenancy(_))
        ));
    }

    #[test]
    fn the_realm_names_the_domain() {
        assert_eq!(domain("ocid1.tenancy.oc1..a").unwrap(), "oraclecloud.com");
        assert_eq!(
            domain("ocid1.tenancy.oc3..a").unwrap(),
            "oraclegovcloud.com"
        );
        assert_eq!(
            domain("ocid1.tenancy.oc4..a").unwrap(),
            "oraclegovcloud.uk"
        );
        assert_eq!(domain("ocid1.tenancy.oc19..a").unwrap(), "oraclecloud.eu");
        assert_eq!(domain("ocid1.tenancy.oc8..a").unwrap(), "oraclecloud8.com");
        assert!(domain("ocid1.user.oc1..a").is_err());
        assert_eq!(realm("ocid1.tenancy.oc2..a").unwrap(), "oc2");
    }

    #[test]
    fn the_caller_is_the_user_else_the_session_subject() {
        let p = parse(TEXT, Utf8Path::new("cfg"), "DEFAULT").unwrap();
        assert_eq!(caller(&p, None), "ocid1.user.oc1..u");
        let s = parse(TEXT, Utf8Path::new("cfg"), "session").unwrap();
        let jwt = format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(r#"{"sub":"ocid1.user.oc1..sso"}"#)
        );
        assert_eq!(caller(&s, Some(&jwt)), "ocid1.user.oc1..sso");
        assert_eq!(caller(&s, Some("junk")), "unknown");
    }

    #[test]
    fn a_profile_signs_with_its_key_and_a_session_with_its_token() {
        let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap()
            .join(format!("precip-oci-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key.pem");
        std::fs::write(&key, test_key::pem()).unwrap();
        let tok = dir.join("token");
        let jwt = format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(r#"{"sub":"ocid1.user.oc1..sso"}"#)
        );
        std::fs::write(&tok, format!("{jwt}\n")).unwrap();
        let text = format!(
            "[DEFAULT]\ntenancy=ocid1.tenancy.oc1..t\nregion=us-ashburn-1\nkey_file={}\nuser=ocid1.user.oc1..u\nfingerprint=ff\n[s]\nuser=\nsecurity_token_file={}\n",
            key, tok
        );
        let p = parse(&text, Utf8Path::new("cfg"), "DEFAULT").unwrap();
        let (key_signer, who) = signer(&p, "DEFAULT").unwrap();
        assert_eq!(who, "ocid1.user.oc1..u");
        assert!(
            format!("{key_signer:?}")
                .contains("ocid1.tenancy.oc1..t/ocid1.user.oc1..u/ff")
        );
        let s = parse(&text, Utf8Path::new("cfg"), "s").unwrap();
        let (session_signer, who) = signer(&s, "s").unwrap();
        assert_eq!(who, "ocid1.user.oc1..sso");
        assert!(format!("{session_signer:?}").contains(&format!("ST${jwt}")));
        let missing = Profile { key_file: dir.join("gone.pem"), ..p };
        assert!(matches!(
            signer(&missing, "DEFAULT"),
            Err(ConfigError::Read(..))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

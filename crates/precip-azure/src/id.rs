//! ARM resource ids. ARM treats ids as case-insensitive and its APIs
//! return them in mixed case (a VM's `managedBy` and the VM's own `id`
//! can differ), so every id precip stores is lowercased first.

use precip_types::Arn;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The stored form of an ARM id.
pub(crate) fn rid(id: &str) -> Arn {
    Arn::from(id.to_lowercase())
}

/// The id up to, not including, `/<segment>/`: a subnet's network, an IP
/// configuration's network interface.
pub(crate) fn parent(id: &str, segment: &str) -> Option<String> {
    let lower = id.to_lowercase();
    let at = lower.find(&format!("/{segment}/"))?;
    Some(lower[..at].to_string())
}

/// The resource group id an id sits under.
pub(crate) fn resource_group(id: &str) -> Option<String> {
    let lower = id.to_lowercase();
    let rest = lower.strip_prefix("/subscriptions/")?;
    let mut parts = rest.splitn(4, '/');
    let sub = parts.next()?;
    (parts.next()? == "resourcegroups").then_some(())?;
    let group = parts.next()?;
    Some(format!("/subscriptions/{sub}/resourcegroups/{group}"))
}

/// The last segment of an id, which is the resource's name.
pub(crate) fn name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// A Resource Graph string column as an optional field. `tostring` of a
/// missing property is the empty string.
pub(crate) fn text(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// A reference to another resource, as ARM nests them: `{"id": "..."}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Ref {
    pub id: String,
}

impl Ref {
    pub(crate) fn rid(&self) -> Option<Arn> {
        text(&self.id).map(|id| rid(&id))
    }
}

/// Tags as Resource Graph returns them: an object, or null when there are
/// none. Values that are not strings are dropped.
pub(crate) fn tags(value: &serde_json::Value) -> BTreeMap<String, String> {
    let Some(obj) = value.as_object() else { return BTreeMap::new() };
    obj.iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// Azure's CPU architecture names in the form the scanners share.
pub(crate) fn architecture(s: &str) -> Option<String> {
    match s.to_ascii_lowercase().as_str() {
        "x64" | "x86_64" => Some("x86_64".into()),
        "arm64" => Some("arm64".into()),
        "" => None,
        other => Some(other.to_string()),
    }
}

/// A Hyper-V generation as a boot mode: generation 1 boots BIOS,
/// generation 2 UEFI.
pub(crate) fn boot_mode(hyper_v_generation: &str) -> Option<String> {
    match hyper_v_generation.to_ascii_uppercase().as_str() {
        "V1" => Some("legacy-bios".into()),
        "V2" => Some("uefi".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIC_IP: &str = "/subscriptions/S1/resourceGroups/Shop/providers/Microsoft.Network/networkInterfaces/web-nic/ipConfigurations/ipconfig1";

    #[test]
    fn ids_are_lowercased_and_split_on_their_segments() {
        assert_eq!(
            rid(NIC_IP).as_str(),
            "/subscriptions/s1/resourcegroups/shop/providers/microsoft.network/networkinterfaces/web-nic/ipconfigurations/ipconfig1"
        );
        assert_eq!(
            parent(NIC_IP, "ipconfigurations").as_deref(),
            Some(
                "/subscriptions/s1/resourcegroups/shop/providers/microsoft.network/networkinterfaces/web-nic"
            )
        );
        assert_eq!(parent(NIC_IP, "subnets"), None);
        assert_eq!(
            resource_group(NIC_IP).as_deref(),
            Some("/subscriptions/s1/resourcegroups/shop")
        );
        assert_eq!(resource_group("/subscriptions/s1"), None);
        assert_eq!(resource_group("not an id"), None);
        assert_eq!(name(NIC_IP), "ipconfig1");
    }

    #[test]
    fn empty_columns_are_missing_and_tags_keep_only_strings() {
        assert_eq!(text("  "), None);
        assert_eq!(text("x").as_deref(), Some("x"));
        assert!(tags(&serde_json::Value::Null).is_empty());
        let t = tags(&serde_json::json!({"app": "shop", "n": 1}));
        assert_eq!(t, BTreeMap::from([("app".into(), "shop".into())]));
        assert_eq!(Ref::default().rid(), None);
    }

    #[test]
    fn architectures_and_generations_map_to_shared_names() {
        assert_eq!(architecture("x64").as_deref(), Some("x86_64"));
        assert_eq!(architecture("Arm64").as_deref(), Some("arm64"));
        assert_eq!(architecture(""), None);
        assert_eq!(boot_mode("V1").as_deref(), Some("legacy-bios"));
        assert_eq!(boot_mode("v2").as_deref(), Some("uefi"));
        assert_eq!(boot_mode(""), None);
    }
}

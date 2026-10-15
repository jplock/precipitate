//! Google Cloud resource names. Cloud Asset Inventory names a resource
//! `//compute.googleapis.com/projects/p/zones/z/instances/i`; the
//! resources themselves refer to each other by self-link
//! (`https://www.googleapis.com/compute/v1/projects/...`) or by a partial
//! name (`projects/p/global/networks/n`). Every id precip stores is the
//! full name.

use precip_types::Arn;
use serde_json::Value;
use std::collections::BTreeMap;

/// The full name of a resource from any of the three forms. A partial
/// name is taken to be Compute Engine's.
pub(crate) fn rid(id: &str) -> Arn {
    Arn::from(full_name(id))
}

fn full_name(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("//") {
        return id.to_string();
    }
    if let Some(rest) = id.strip_prefix("https://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        // www.googleapis.com/compute/v1/... and
        // compute.googleapis.com/compute/v1/... name the service in the
        // path; other hosts are the service, with the version first.
        let (service, path) = match host {
            "www.googleapis.com" => match path.split_once('/') {
                Some((service, rest)) => {
                    (format!("{service}.googleapis.com"), rest)
                }
                None => (host.to_string(), path),
            },
            h => {
                let service = h.split('.').next().unwrap_or(h);
                let path =
                    path.strip_prefix(&format!("{service}/")).unwrap_or(path);
                (h.to_string(), path)
            }
        };
        let path = path.split_once('/').map_or(path, |(_version, rest)| rest);
        return format!("//{service}/{path}");
    }
    format!("//compute.googleapis.com/{}", id.trim_start_matches('/'))
}

/// The last segment of a name, which is the resource's own name.
pub(crate) fn name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The segment after `projects/` in a name.
pub(crate) fn project(id: &str) -> Option<&str> {
    let (_, rest) = id.split_once("projects/")?;
    rest.split('/').next().filter(|p| !p.is_empty())
}

/// A zone's region: `us-central1-a` is in `us-central1`. Regions and
/// `global` are their own.
pub(crate) fn region(location: &str) -> String {
    let location = location.trim().to_lowercase();
    if location.is_empty() {
        return "global".into();
    }
    match location.rsplit_once('-') {
        Some((region, zone))
            if location.matches('-').count() == 2 && zone.len() == 1 =>
        {
            region.to_string()
        }
        Some(_) | None => location,
    }
}

/// A string field, or `None` when missing or blank.
pub(crate) fn text(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub(crate) fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer).and_then(Value::as_str).and_then(text)
}

/// A number at `pointer`. The REST APIs send 64-bit integers as strings.
pub(crate) fn int_at(v: &Value, pointer: &str) -> Option<i64> {
    let at = v.pointer(pointer)?;
    at.as_i64()
        .or_else(|| at.as_str()?.trim().parse().ok())
        .or_else(|| at.as_f64().map(|f| f.round() as i64))
}

pub(crate) fn i32_at(v: &Value, pointer: &str) -> Option<i32> {
    int_at(v, pointer).and_then(|n| i32::try_from(n).ok())
}

/// The strings of an array at `pointer`.
pub(crate) fn strs_at<'a>(v: &'a Value, pointer: &str) -> Vec<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The objects of an array at `pointer`.
pub(crate) fn items_at<'a>(v: &'a Value, pointer: &str) -> &'a [Value] {
    v.pointer(pointer).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Labels as tags. Values that are not strings are dropped.
pub(crate) fn labels(value: Option<&Value>) -> BTreeMap<String, String> {
    let Some(obj) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// Compute Engine's CPU architecture names in the form the scanners
/// share.
pub(crate) fn architecture(s: &str) -> Option<String> {
    match s.to_ascii_uppercase().as_str() {
        "X86_64" => Some("x86_64".into()),
        "ARM64" => Some("arm64".into()),
        "" | "ARCHITECTURE_UNSPECIFIED" => None,
        other => Some(other.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_of_a_reference_becomes_the_full_name() {
        let full =
            "//compute.googleapis.com/projects/p/zones/us-central1-a/disks/d";
        assert_eq!(rid(full).as_str(), full);
        assert_eq!(
            rid("https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a/disks/d")
                .as_str(),
            full
        );
        assert_eq!(
            rid("https://compute.googleapis.com/compute/beta/projects/p/zones/us-central1-a/disks/d")
                .as_str(),
            full
        );
        assert_eq!(
            rid("projects/p/zones/us-central1-a/disks/d").as_str(),
            full
        );
        assert_eq!(
            rid("https://container.googleapis.com/v1/projects/p/locations/l/clusters/c").as_str(),
            "//container.googleapis.com/projects/p/locations/l/clusters/c"
        );
    }

    #[test]
    fn names_and_projects_split_out() {
        let id = "//compute.googleapis.com/projects/shop/regions/us-east1/routers/r/nats/n";
        assert_eq!(name(id), "n");
        assert_eq!(project(id), Some("shop"));
        assert_eq!(project("//x/global/networks/n"), None);
    }

    #[test]
    fn zones_fold_into_their_region() {
        assert_eq!(region("us-central1-a"), "us-central1");
        assert_eq!(
            region("northamerica-northeast1-b"),
            "northamerica-northeast1"
        );
        assert_eq!(region("europe-west4"), "europe-west4");
        assert_eq!(
            region("northamerica-northeast1"),
            "northamerica-northeast1"
        );
        assert_eq!(region("global"), "global");
        assert_eq!(region(""), "global");
        assert_eq!(region("US"), "us");
    }

    #[test]
    fn numbers_arrive_as_strings_or_numbers() {
        let v = serde_json::json!({"a": "100", "b": 7, "c": "x", "d": 2.6});
        assert_eq!(int_at(&v, "/a"), Some(100));
        assert_eq!(int_at(&v, "/b"), Some(7));
        assert_eq!(int_at(&v, "/c"), None);
        assert_eq!(int_at(&v, "/d"), Some(3));
        assert_eq!(int_at(&v, "/missing"), None);
    }

    #[test]
    fn labels_keep_only_strings_and_architectures_normalise() {
        let l = labels(Some(&serde_json::json!({"app": "shop", "n": 1})));
        assert_eq!(l, BTreeMap::from([("app".into(), "shop".into())]));
        assert!(labels(None).is_empty());
        assert_eq!(architecture("X86_64").as_deref(), Some("x86_64"));
        assert_eq!(architecture("ARM64").as_deref(), Some("arm64"));
        assert_eq!(architecture("ARCHITECTURE_UNSPECIFIED"), None);
    }
}

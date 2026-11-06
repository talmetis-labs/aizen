//! Versioned creative-production messages, not an application transport or execution authority.
//! Validation is pure: roots are only checked syntactically; adapters must resolve paths, reject
//! symlink escapes and establish trustworthy evidence separately. Matching identities do not do that.
//! Callers must bound incoming bytes before serde: field/count checks are not an allocation firewall.

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
const MAX_CAPABILITIES: usize = 16;
const MAX_OPERATIONS: usize = 32;
const MAX_BATCH_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 256;
const MAX_VERSION_BYTES: usize = 64;
const MAX_ROOT_BYTES: usize = 4096;
const MAX_RELATIVE_PATH_BYTES: usize = 2048;
const MAX_PATH_COMPONENTS: usize = 32;
const MAX_TRANSFORM_MAGNITUDE: f64 = 1_000_000_000.0;
const MAX_FRAME: u32 = 1_000_000;
const MAX_RENDER_DIMENSION: u32 = 8192;
const MAX_RENDER_PIXELS: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppKind {
    Blender,
    Unity,
    Unreal,
    Spine,
}

/// Frozen selection identity only; neither this binding nor its digest grants effect/evidence rights.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetBinding {
    pub(crate) adapter_id: String,
    pub(crate) adapter_version: String,
    pub(crate) app: AppKind,
    pub(crate) app_version: String,
    pub(crate) instance_epoch: String,
    pub(crate) project_root: PathBuf,
    pub(crate) document_id: String,
    pub(crate) revision: String,
    pub(crate) schema_digest: String,
}

impl TargetBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        stable_id(&self.adapter_id, "adapter_id")?;
        version_text(&self.adapter_version, "adapter_version")?;
        version_text(&self.app_version, "app_version")?;
        stable_id(&self.instance_epoch, "instance_epoch")?;
        stable_id(&self.document_id, "document_id")?;
        stable_id(&self.revision, "revision")?;
        ensure!(
            self.schema_digest.len() == 64
                && self
                    .schema_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "schema_digest must be a lowercase SHA-256 hex digest"
        );
        let root = self
            .project_root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("project_root must be UTF-8"))?;
        safe_text(root, "project_root", MAX_ROOT_BYTES)?;
        ensure!(
            self.project_root.is_absolute(),
            "project_root must be syntactically absolute on this host"
        );
        Ok(())
    }

    fn matches_exactly(&self, other: &Self) -> bool {
        // Path equality normalizes some spelling differences; a frozen binding must not do so.
        self == other && self.project_root.as_os_str() == other.project_root.as_os_str()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    SetTransform,
    AssignMaterial,
    ImportAsset,
    ExportAsset,
    RenderStill,
    RunScenario,
}

/// A closed native operation vocabulary. There is deliberately no script, eval, or shell payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AppOperation {
    SetTransform {
        object_id: String,
        translation: [f64; 3],
        rotation: [f64; 3],
        scale: [f64; 3],
    },
    AssignMaterial {
        object_id: String,
        material_id: String,
    },
    ImportAsset {
        source: String,
    },
    ExportAsset {
        object_id: String,
        preset_id: String,
        output: String,
    },
    RenderStill {
        camera_id: String,
        frame: u32,
        width: u32,
        height: u32,
        output: String,
    },
    RunScenario {
        scenario_id: String,
        seed: u64,
    },
}

impl AppOperation {
    pub(crate) fn kind(&self) -> OperationKind {
        match self {
            Self::SetTransform { .. } => OperationKind::SetTransform,
            Self::AssignMaterial { .. } => OperationKind::AssignMaterial,
            Self::ImportAsset { .. } => OperationKind::ImportAsset,
            Self::ExportAsset { .. } => OperationKind::ExportAsset,
            Self::RenderStill { .. } => OperationKind::RenderStill,
            Self::RunScenario { .. } => OperationKind::RunScenario,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::SetTransform {
                object_id,
                translation,
                rotation,
                scale,
            } => {
                stable_id(object_id, "object_id")?;
                // App-native units/conventions belong to the versioned adapter schema. Zero and
                // negative scales are valid native values (flatten/mirror), not execution policy.
                for values in [translation, rotation, scale] {
                    ensure!(
                        values
                            .iter()
                            .all(|v| v.is_finite() && v.abs() <= MAX_TRANSFORM_MAGNITUDE),
                        "transform components must be finite and within magnitude bounds"
                    );
                }
            }
            Self::AssignMaterial {
                object_id,
                material_id,
            } => {
                stable_id(object_id, "object_id")?;
                stable_id(material_id, "material_id")?;
            }
            Self::ImportAsset { source } => relative_path(source)?,
            Self::ExportAsset {
                object_id,
                preset_id,
                output,
            } => {
                stable_id(object_id, "object_id")?;
                stable_id(preset_id, "preset_id")?;
                relative_path(output)?;
            }
            Self::RenderStill {
                camera_id,
                frame,
                width,
                height,
                output,
            } => {
                stable_id(camera_id, "camera_id")?;
                ensure!(*frame <= MAX_FRAME, "render frame exceeds bound");
                ensure!(
                    (1..=MAX_RENDER_DIMENSION).contains(width)
                        && (1..=MAX_RENDER_DIMENSION).contains(height)
                        && u64::from(*width) * u64::from(*height) <= MAX_RENDER_PIXELS,
                    "render dimensions exceed bounds"
                );
                relative_path(output)?;
            }
            Self::RunScenario { scenario_id, .. } => stable_id(scenario_id, "scenario_id")?,
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilityManifest {
    pub(crate) protocol_version: u32,
    pub(crate) target: TargetBinding,
    #[serde(deserialize_with = "deserialize_capabilities")]
    pub(crate) operations: Vec<OperationKind>,
    pub(crate) supports_cancel: bool,
}

impl CapabilityManifest {
    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_BATCH_BYTES,
            "capability JSON exceeds 64 KiB"
        );
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid app capability JSON"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        protocol_version(self.protocol_version)?;
        self.target.validate()?;
        ensure!(
            self.operations.len() <= MAX_CAPABILITIES,
            "capability count exceeds bound"
        );
        for (index, operation) in self.operations.iter().enumerate() {
            ensure!(
                !self.operations[..index].contains(operation),
                "duplicate capability"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchRequest {
    pub(crate) protocol_version: u32,
    pub(crate) target: TargetBinding,
    #[serde(deserialize_with = "deserialize_operations")]
    pub(crate) operations: Vec<AppOperation>,
}

impl BatchRequest {
    pub(crate) fn from_slice(bytes: &[u8], manifest: &CapabilityManifest) -> Result<Self> {
        ensure!(bytes.len() <= MAX_BATCH_BYTES, "batch input exceeds 64 KiB");
        let batch: Self =
            serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid app batch JSON"))?;
        batch.validate(manifest)?;
        Ok(batch)
    }

    pub(crate) fn validate(&self, manifest: &CapabilityManifest) -> Result<()> {
        manifest.validate()?;
        self.validated_bytes()?;
        ensure!(
            self.target.matches_exactly(&manifest.target),
            "batch target differs from frozen capability target"
        );
        ensure!(
            self.operations
                .iter()
                .all(|op| manifest.operations.contains(&op.kind())),
            "batch contains an unsupported operation"
        );
        Ok(())
    }

    /// Hashes bounded, intrinsically valid versioned JSON, not adapter authorization or evidence.
    /// Capability/target freshness still requires `validate` against the controller's frozen manifest.
    pub(crate) fn digest(&self) -> Result<String> {
        Ok(crate::core::tool_media::sha256(&self.validated_bytes()?))
    }

    fn validated_bytes(&self) -> Result<Vec<u8>> {
        protocol_version(self.protocol_version)?;
        self.target.validate()?;
        ensure!(
            (1..=MAX_OPERATIONS).contains(&self.operations.len()),
            "batch requires 1..=32 operations"
        );
        for operation in &self.operations {
            operation.validate()?;
        }
        // In particular, validate finite floats BEFORE serde_json (which serializes NaN as null).
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= MAX_BATCH_BYTES, "batch JSON exceeds 64 KiB");
        Ok(bytes)
    }
}

fn protocol_version(version: u32) -> Result<()> {
    ensure!(
        version == PROTOCOL_VERSION,
        "unsupported app protocol version"
    );
    Ok(())
}

fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn safe_text(value: &str, field: &str, max_bytes: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= max_bytes
            && value.trim() == value
            && !value.chars().any(unsafe_char),
        "invalid or oversized {field}"
    );
    let lower = value.to_ascii_lowercase();
    // Reject endpoint/credential-shaped metadata, not just terminal escapes. This is deliberately
    // not a promise to detect arbitrary secrets; IDs must also obey the narrow opaque-ID grammar.
    ensure!(
        ![
            "://",
            "bearer ",
            "api_key=",
            "api-key=",
            "apikey=",
            "token=",
            "token:",
            "secret=",
            "secret:",
            "password=",
            "password:",
            "-----begin"
        ]
        .iter()
        .any(|marker| lower.contains(marker))
            && ![
                "sk-",
                "sk_",
                "ghp_",
                "gho_",
                "ghu_",
                "ghs_",
                "github_pat_",
                "xoxb-",
                "xoxp-"
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix)),
        "endpoint or credential-shaped {field} is not allowed"
    );
    Ok(())
}

fn stable_id(value: &str, field: &str) -> Result<()> {
    safe_text(value, field, MAX_ID_BYTES)?;
    ensure!(
        value != "."
            && value != ".."
            && value.chars().all(|c| {
                (!c.is_whitespace() || c == ' ')
                    && !matches!(c, '/' | '\\' | ':' | '@' | '?' | '#' | '&' | '=' | '%')
            }),
        "{field} must be an opaque identifier, not an endpoint"
    );
    Ok(())
}

fn version_text(value: &str, field: &str) -> Result<()> {
    safe_text(value, field, MAX_VERSION_BYTES)?;
    ensure!(
        value.bytes().any(|b| b.is_ascii_alphanumeric())
            && value.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b' ')
            }),
        "invalid {field} syntax"
    );
    Ok(())
}

/// Slash-separated relative paths on every host, not `Path::components` on only the current OS.
/// This is lexical validation only; it does not prove containment in the presence of symlinks.
fn relative_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_RELATIVE_PATH_BYTES
            && !value.starts_with('/')
            && !value
                .chars()
                .any(|c| unsafe_char(c)
                    || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')),
        "invalid or oversized relative app path"
    );
    let mut components = 0;
    for component in value.split('/') {
        components += 1;
        ensure!(
            components <= MAX_PATH_COMPONENTS
                && !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= 255
                && component.trim() == component
                && !component.ends_with('.')
                && !windows_reserved_name(component),
            "invalid relative app path component"
        );
    }
    Ok(())
}

fn windows_reserved_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(*prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn deserialize_capabilities<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<OperationKind>, D::Error> {
    deserialize_bounded_vec::<D, OperationKind, MAX_CAPABILITIES>(deserializer)
}

fn deserialize_operations<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<AppOperation>, D::Error> {
    deserialize_bounded_vec::<D, AppOperation, MAX_OPERATIONS>(deserializer)
}

fn deserialize_bounded_vec<'de, D, T, const MAX: usize>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "an array with at most {MAX} entries")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
            while let Some(value) = seq.next_element()? {
                if values.len() == MAX {
                    return Err(serde::de::Error::custom(
                        "app operation count exceeds bound",
                    ));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, MAX>(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn target() -> TargetBinding {
        TargetBinding {
            adapter_id: "aizen.blender".into(),
            adapter_version: "1.0.0+test".into(),
            app: AppKind::Blender,
            app_version: "4.2.1 LTS".into(),
            instance_epoch: "epoch-1234".into(),
            project_root: PathBuf::from(if cfg!(windows) {
                "C:/creative/project"
            } else {
                "/creative/project"
            }),
            document_id: "scene-001".into(),
            revision: "rev-7".into(),
            schema_digest: "a".repeat(64),
        }
    }

    fn manifest() -> CapabilityManifest {
        CapabilityManifest {
            protocol_version: PROTOCOL_VERSION,
            target: target(),
            operations: vec![
                OperationKind::SetTransform,
                OperationKind::AssignMaterial,
                OperationKind::ImportAsset,
                OperationKind::ExportAsset,
                OperationKind::RenderStill,
                OperationKind::RunScenario,
            ],
            supports_cancel: true,
        }
    }

    fn transform() -> AppOperation {
        AppOperation::SetTransform {
            object_id: "mesh-1".into(),
            translation: [0.0, 1.0, -2.0],
            rotation: [0.0; 3],
            scale: [1.0, 0.0, -1.0],
        }
    }

    fn render() -> AppOperation {
        AppOperation::RenderStill {
            camera_id: "camera-1".into(),
            frame: 0,
            width: 1920,
            height: 1080,
            output: "renders/still.png".into(),
        }
    }

    fn operations() -> Vec<AppOperation> {
        vec![
            transform(),
            AppOperation::AssignMaterial {
                object_id: "mesh-1".into(),
                material_id: "material-1".into(),
            },
            AppOperation::ImportAsset {
                source: "assets/nhân vật.fbx".into(),
            },
            AppOperation::ExportAsset {
                object_id: "mesh-1".into(),
                preset_id: "gltf-2".into(),
                output: "exports/mesh.glb".into(),
            },
            render(),
            AppOperation::RunScenario {
                scenario_id: "smoke-test".into(),
                seed: u64::MAX,
            },
        ]
    }

    fn batch() -> BatchRequest {
        BatchRequest {
            protocol_version: PROTOCOL_VERSION,
            target: target(),
            operations: operations(),
        }
    }

    #[test]
    fn bounded_entrypoints_reject_raw_oversize_before_deserialization() {
        let oversize = vec![b' '; MAX_BATCH_BYTES + 1];
        assert!(CapabilityManifest::from_slice(&oversize).is_err());
        assert!(BatchRequest::from_slice(&oversize, &manifest()).is_err());
        let capabilities =
            CapabilityManifest::from_slice(&serde_json::to_vec(&manifest()).unwrap()).unwrap();
        let request =
            BatchRequest::from_slice(&serde_json::to_vec(&batch()).unwrap(), &capabilities)
                .unwrap();
        assert_eq!(request, batch());
        let mut stale = capabilities;
        stale.target.revision = "changed".into();
        assert!(BatchRequest::from_slice(&serde_json::to_vec(&batch()).unwrap(), &stale).is_err());
        let error =
            CapabilityManifest::from_slice(br#"{"protocol_version":"private-value"}"#).unwrap_err();
        assert!(!error.to_string().contains("private-value"));
    }

    #[test]
    fn all_operations_validate_and_round_trip() {
        let expected = batch();
        expected.validate(&manifest()).unwrap();
        let bytes = serde_json::to_vec(&expected).unwrap();
        let decoded: BatchRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(
            decoded
                .operations
                .iter()
                .map(AppOperation::kind)
                .collect::<Vec<_>>(),
            manifest().operations
        );
    }

    #[test]
    fn app_names_are_closed_snake_case() {
        for (app, name) in [
            (AppKind::Blender, "blender"),
            (AppKind::Unity, "unity"),
            (AppKind::Unreal, "unreal"),
            (AppKind::Spine, "spine"),
        ] {
            assert_eq!(serde_json::to_value(app).unwrap(), json!(name));
            assert_eq!(serde_json::from_value::<AppKind>(json!(name)).unwrap(), app);
        }
        for name in ["Blender", "other", "eval"] {
            assert!(serde_json::from_value::<AppKind>(json!(name)).is_err());
        }
    }

    #[test]
    fn every_object_rejects_unknown_fields() {
        let mut value = serde_json::to_value(target()).unwrap();
        value["endpoint"] = json!("redacted");
        assert!(serde_json::from_value::<TargetBinding>(value).is_err());
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<CapabilityManifest>(value).is_err());
        let mut value = serde_json::to_value(batch()).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<BatchRequest>(value).is_err());
        for operation in operations() {
            let mut value = serde_json::to_value(operation).unwrap();
            value["script"] = json!("not a supported payload");
            assert!(serde_json::from_value::<AppOperation>(value).is_err());
        }
    }

    #[test]
    fn unknown_operations_and_eval_are_not_a_language() {
        for kind in [
            "eval",
            "script",
            "python",
            "shell",
            "run_script",
            "SetTransform",
        ] {
            assert!(serde_json::from_value::<OperationKind>(json!(kind)).is_err());
            assert!(serde_json::from_value::<AppOperation>(json!({"kind": kind})).is_err());
        }
        assert!(serde_json::from_value::<AppOperation>(json!({
            "kind": "run_scenario", "scenario_id": "safe", "seed": 1, "code": "print(1)"
        }))
        .is_err());
    }

    #[test]
    fn required_fields_have_no_implicit_defaults() {
        for mut value in [
            serde_json::to_value(target()).unwrap(),
            serde_json::to_value(manifest()).unwrap(),
            serde_json::to_value(batch()).unwrap(),
        ] {
            let key = if value.get("adapter_id").is_some() {
                "revision"
            } else {
                "protocol_version"
            };
            value.as_object_mut().unwrap().remove(key);
            if key == "revision" {
                assert!(serde_json::from_value::<TargetBinding>(value).is_err());
            } else if value.get("supports_cancel").is_some() {
                assert!(serde_json::from_value::<CapabilityManifest>(value).is_err());
            } else {
                assert!(serde_json::from_value::<BatchRequest>(value).is_err());
            }
        }
        for mut value in operations()
            .into_iter()
            .map(|op| serde_json::to_value(op).unwrap())
        {
            let key = value
                .as_object()
                .unwrap()
                .keys()
                .find(|key| *key != "kind")
                .unwrap()
                .clone();
            value.as_object_mut().unwrap().remove(&key);
            assert!(serde_json::from_value::<AppOperation>(value).is_err());
        }
    }

    #[test]
    fn unsupported_versions_fail_validation_and_digest() {
        for version in [0, 2, u32::MAX] {
            let mut capabilities = manifest();
            capabilities.protocol_version = version;
            assert!(capabilities.validate().is_err());
            assert!(batch().validate(&capabilities).is_err());
            let mut request = batch();
            request.protocol_version = version;
            assert!(request.validate(&manifest()).is_err());
            assert!(request.digest().is_err());
        }
    }

    #[test]
    fn duplicate_and_oversized_capabilities_fail() {
        let mut capabilities = manifest();
        capabilities.operations.push(OperationKind::RenderStill);
        assert!(capabilities.validate().is_err());
        capabilities.operations = vec![OperationKind::SetTransform; MAX_CAPABILITIES + 1];
        assert!(capabilities.validate().is_err());
        let value = serde_json::to_value(capabilities).unwrap();
        assert!(serde_json::from_value::<CapabilityManifest>(value).is_err());
    }

    #[test]
    fn empty_capabilities_are_valid_but_authorize_nothing() {
        let mut capabilities = manifest();
        capabilities.operations.clear();
        capabilities.validate().unwrap();
        assert!(batch().validate(&capabilities).is_err());
    }

    #[test]
    fn every_target_field_is_frozen_including_epoch_revision_and_schema() {
        let baseline = serde_json::to_value(target()).unwrap();
        for (field, replacement) in [
            ("adapter_id", json!("other-adapter")),
            ("adapter_version", json!("2.0")),
            ("app", json!("unity")),
            ("app_version", json!("5.0")),
            ("instance_epoch", json!("new-epoch")),
            ("project_root", json!(target().project_root.join("other"))),
            ("document_id", json!("other-document")),
            ("revision", json!("rev-8")),
            ("schema_digest", json!("b".repeat(64))),
        ] {
            let mut value = baseline.clone();
            value[field] = replacement;
            let mut request = batch();
            request.target = serde_json::from_value(value).unwrap();
            request.target.validate().unwrap();
            assert!(request.validate(&manifest()).is_err(), "{field}");
        }
    }

    #[test]
    fn project_root_spelling_is_not_normalized_to_match() {
        let mut request = batch();
        let root = request.target.project_root.to_str().unwrap();
        request.target.project_root = PathBuf::from(format!("{root}/."));
        request.target.validate().unwrap();
        assert!(request.validate(&manifest()).is_err());
    }

    #[test]
    fn capabilities_are_checked_for_each_operation() {
        for missing in manifest().operations {
            let mut capabilities = manifest();
            capabilities.operations.retain(|kind| *kind != missing);
            assert!(batch().validate(&capabilities).is_err(), "{missing:?}");
        }
    }

    #[test]
    fn batch_count_bounds_apply_to_deserialized_and_constructed_values() {
        let mut request = batch();
        request.operations = vec![transform(); MAX_OPERATIONS];
        request.validate(&manifest()).unwrap();
        serde_json::from_value::<BatchRequest>(serde_json::to_value(&request).unwrap()).unwrap();
        request.operations.push(transform());
        assert!(request.validate(&manifest()).is_err());
        assert!(request.digest().is_err());
        assert!(
            serde_json::from_value::<BatchRequest>(serde_json::to_value(&request).unwrap())
                .is_err()
        );
        request.operations.clear();
        assert!(request.validate(&manifest()).is_err());
        assert!(request.digest().is_err());
    }

    #[test]
    fn serialized_batch_byte_bound_is_independent_of_operation_count() {
        let output = vec!["a".repeat(240); 8].join("/");
        let operation = AppOperation::ExportAsset {
            object_id: "o".repeat(MAX_ID_BYTES),
            preset_id: "p".repeat(MAX_ID_BYTES),
            output,
        };
        operation.validate().unwrap();
        let mut request = batch();
        request.operations = vec![operation; MAX_OPERATIONS];
        assert!(serde_json::to_vec(&request).unwrap().len() > MAX_BATCH_BYTES);
        assert!(request.validate(&manifest()).is_err());
        assert!(request.digest().is_err());
    }

    #[test]
    fn nonfinite_transforms_cannot_validate_or_hash_as_null() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for field in 0..3 {
                for component in 0..3 {
                    let mut operation = transform();
                    if let AppOperation::SetTransform {
                        translation,
                        rotation,
                        scale,
                        ..
                    } = &mut operation
                    {
                        [translation, rotation, scale][field][component] = value;
                    }
                    assert!(operation.validate().is_err());
                    let mut request = batch();
                    request.operations = vec![operation];
                    assert!(request.validate(&manifest()).is_err());
                    assert!(request.digest().is_err());
                }
            }
        }
    }

    #[test]
    fn transforms_are_fixed_length_and_magnitude_bounded() {
        let mut value = serde_json::to_value(transform()).unwrap();
        for array in [json!([0, 0]), json!([0, 0, 0, 0]), json!([null, 0, 0])] {
            value["translation"] = array;
            assert!(serde_json::from_value::<AppOperation>(value.clone()).is_err());
        }
        let mut operation = transform();
        if let AppOperation::SetTransform { translation, .. } = &mut operation {
            translation[0] = MAX_TRANSFORM_MAGNITUDE;
        }
        operation.validate().unwrap();
        if let AppOperation::SetTransform { translation, .. } = &mut operation {
            translation[0] = MAX_TRANSFORM_MAGNITUDE + 1.0;
        }
        assert!(operation.validate().is_err());
    }

    #[test]
    fn render_size_frame_and_pixel_bounds_are_checked_without_overflow() {
        for (width, height, frame) in [
            (0, 1080, 0),
            (1920, 0, 0),
            (MAX_RENDER_DIMENSION + 1, 1, 0),
            (1, MAX_RENDER_DIMENSION + 1, 0),
            (MAX_RENDER_DIMENSION, MAX_RENDER_DIMENSION, 0),
            (u32::MAX, u32::MAX, 0),
            (1920, 1080, MAX_FRAME + 1),
        ] {
            let operation = AppOperation::RenderStill {
                camera_id: "camera-1".into(),
                frame,
                width,
                height,
                output: "still.png".into(),
            };
            assert!(operation.validate().is_err(), "{width}x{height}@{frame}");
        }
        AppOperation::RenderStill {
            camera_id: "camera-1".into(),
            frame: MAX_FRAME,
            width: MAX_RENDER_DIMENSION,
            height: 4096,
            output: "still.png".into(),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn numeric_fields_do_not_accept_scripts_negative_or_fractional_values() {
        for field in ["frame", "width", "height"] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!("1"),
                json!(null),
                json!(u64::MAX),
            ] {
                let mut value = serde_json::to_value(render()).unwrap();
                value[field] = invalid;
                assert!(serde_json::from_value::<AppOperation>(value).is_err());
            }
        }
        for invalid in [json!(-1), json!(1.5), json!("random"), json!(null)] {
            assert!(serde_json::from_value::<AppOperation>(json!({
                "kind": "run_scenario", "scenario_id": "test", "seed": invalid
            }))
            .is_err());
        }
        assert!(serde_json::from_value::<AppOperation>(json!({
            "kind": "run_scenario", "scenario_id": "test"
        }))
        .is_err());
    }

    #[test]
    fn relative_paths_reject_portable_windows_and_traversal_pitfalls() {
        for path in [
            "",
            "/absolute",
            "//server/share",
            "C:/asset.fbx",
            "C:asset.fbx",
            r"C:\asset.fbx",
            r"\rooted",
            r"\\server\share\asset",
            r"\\?\C:\asset",
            r"folder\asset",
            "../asset",
            "a/../asset",
            "./asset",
            "a/./asset",
            "a//asset",
            "a/",
            "a/file:stream",
            "a/CON",
            "a/nul.txt",
            "PRN.bin",
            "AUX",
            "com1",
            "LPT9.ext",
            "COM¹.txt",
            "lpt²",
            "CON .txt",
            "CLOCK$",
            "CONIN$",
            "CONOUT$.txt",
            "a/file.",
            "a/file ",
            " leading/file",
            "a/ file",
            "a/<file",
            "a/>file",
            "a/\"file",
            "a/|file",
            "a/?file",
            "a/*file",
            "a/\0file",
            "a/\nfile",
            "a/\u{1b}file",
            "a/\u{202e}file",
            "https://host/file",
        ] {
            assert!(relative_path(path).is_err(), "{path:?}");
        }
        for path in [
            "asset.fbx",
            "assets/Cube.001.fbx",
            "assets/nhân vật.fbx",
            "a/com10.txt",
            "a/console.txt",
        ] {
            relative_path(path).unwrap();
        }
    }

    #[test]
    fn path_bounds_cover_bytes_components_and_component_size() {
        let max_components = vec!["a"; MAX_PATH_COMPONENTS].join("/");
        relative_path(&max_components).unwrap();
        assert!(relative_path(&format!("{max_components}/a")).is_err());
        relative_path(&"a".repeat(255)).unwrap();
        assert!(relative_path(&"a".repeat(256)).is_err());
        let max_path = format!("{}/{}", vec!["a".repeat(255); 7].join("/"), "b".repeat(255));
        assert_eq!(max_path.len(), MAX_RELATIVE_PATH_BYTES - 1);
        relative_path(&max_path).unwrap();
        let at_limit = format!("{}/{}", vec!["a".repeat(240); 8].join("/"), "b".repeat(120));
        assert_eq!(at_limit.len(), MAX_RELATIVE_PATH_BYTES);
        relative_path(&at_limit).unwrap();
        assert!(relative_path(&format!("{at_limit}b")).is_err());
        assert!(relative_path(&"é".repeat(128)).is_err());
    }

    #[test]
    fn every_path_payload_is_validated() {
        for operation in [
            AppOperation::ImportAsset {
                source: "../escape".into(),
            },
            AppOperation::ExportAsset {
                object_id: "obj".into(),
                preset_id: "preset".into(),
                output: "C:/escape".into(),
            },
            AppOperation::RenderStill {
                camera_id: "camera".into(),
                frame: 0,
                width: 1,
                height: 1,
                output: "a/file:ads".into(),
            },
        ] {
            assert!(operation.validate().is_err());
            let mut request = batch();
            request.operations = vec![operation];
            assert!(request.digest().is_err());
        }
    }

    #[test]
    fn metadata_is_bounded_safe_and_not_a_transport_endpoint() {
        for invalid in [
            "",
            " ",
            " padded",
            "padded ",
            ".",
            "..",
            "id\0x",
            "id\nx",
            "id\u{1b}x",
            "id\u{202e}x",
            "https://user:pass@host",
            "ws://127.0.0.1:4000",
            "127.0.0.1:4000",
            "localhost:5000",
            "user:password@host",
            "sk-example-key",
            "SK-example-key",
            "ghp_example",
            "github_pat_example",
            "xoxb-example",
            "Bearer credential",
            "token=credential",
            "api_key=credential",
            "secret:credential",
        ] {
            let mut binding = target();
            binding.instance_epoch = invalid.into();
            assert!(binding.validate().is_err(), "{invalid:?}");
        }
        let mut binding = target();
        binding.document_id = "a".repeat(MAX_ID_BYTES);
        binding.validate().unwrap();
        binding.document_id.push('a');
        assert!(binding.validate().is_err());
        let oversized_version = "1".repeat(MAX_VERSION_BYTES + 1);
        for invalid in [
            "",
            " ",
            "1.0\n",
            "https://host",
            "sk-example",
            oversized_version.as_str(),
        ] {
            let mut binding = target();
            binding.adapter_version = invalid.into();
            assert!(binding.validate().is_err());
        }
    }

    #[test]
    fn unicode_document_and_object_names_are_opaque_not_ascii_only() {
        let mut binding = target();
        binding.document_id = "Cảnh chính (Preview)".into();
        binding.revision = "bản-2".into();
        binding.validate().unwrap();
        AppOperation::AssignMaterial {
            object_id: "Nhân vật (1)".into(),
            material_id: "Vật liệu.001".into(),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn schema_hash_requires_exactly_64_lower_hex_characters() {
        for invalid in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            " ".repeat(64),
        ] {
            let mut binding = target();
            binding.schema_digest = invalid;
            assert!(binding.validate().is_err());
        }
        let mut binding = target();
        binding.schema_digest = "0123456789abcdef".repeat(4);
        binding.validate().unwrap();
    }

    #[test]
    fn root_validation_is_syntactic_without_requiring_a_real_directory() {
        target().validate().unwrap();
        for invalid in ["relative/project", "", "https://host/project", "C:relative"] {
            let mut binding = target();
            binding.project_root = PathBuf::from(invalid);
            assert!(binding.validate().is_err(), "{invalid:?}");
        }
        let mut binding = target();
        binding.project_root = PathBuf::from(format!("{}\0x", binding.project_root.display()));
        assert!(binding.validate().is_err());
        let mut binding = target();
        binding.project_root = binding.project_root.join("a".repeat(MAX_ROOT_BYTES));
        assert!(binding.validate().is_err());
    }

    #[test]
    fn every_operation_identifier_is_checked() {
        for mut value in operations()
            .into_iter()
            .map(|op| serde_json::to_value(op).unwrap())
        {
            let fields: Vec<String> = value
                .as_object()
                .unwrap()
                .keys()
                .filter(|key| key.ends_with("_id"))
                .cloned()
                .collect();
            for field in fields {
                let original = value[&field].clone();
                value[&field] = json!(" ");
                let operation: AppOperation = serde_json::from_value(value.clone()).unwrap();
                assert!(operation.validate().is_err(), "{field}");
                value[&field] = original;
            }
        }
    }

    #[test]
    fn digest_is_repeatable_and_binds_the_complete_request() {
        let request = batch();
        let first = request.digest().unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(first, request.clone().digest().unwrap());
        assert_eq!(
            first,
            crate::core::tool_media::sha256(&serde_json::to_vec(&request).unwrap())
        );
        let mut changed = request.clone();
        changed.target.instance_epoch = "another-epoch".into();
        assert_ne!(first, changed.digest().unwrap());
        let mut changed = request.clone();
        changed.operations.reverse();
        assert_ne!(first, changed.digest().unwrap());
        let mut changed = request;
        changed.operations.push(AppOperation::RunScenario {
            scenario_id: "test".into(),
            seed: 42,
        });
        assert_ne!(first, changed.digest().unwrap());
    }

    #[test]
    fn duplicate_json_keys_are_rejected() {
        assert!(serde_json::from_str::<AppOperation>(
            r#"{"kind":"import_asset","source":"a","source":"b"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<AppOperation>(
            r#"{"kind":"import_asset","kind":"run_scenario","source":"a"}"#
        )
        .is_err());
    }

    #[test]
    fn numeric_overflow_and_non_json_floats_are_rejected_by_serde() {
        for input in [
            r#"{"kind":"set_transform","object_id":"obj","translation":[NaN,0,0],"rotation":[0,0,0],"scale":[1,1,1]}"#,
            r#"{"kind":"set_transform","object_id":"obj","translation":[1e999,0,0],"rotation":[0,0,0],"scale":[1,1,1]}"#,
            r#"{"kind":"run_scenario","scenario_id":"test","seed":18446744073709551616}"#,
        ] {
            assert!(serde_json::from_str::<AppOperation>(input).is_err());
        }
        let value: Value = serde_json::to_value(transform()).unwrap();
        assert!(value["translation"].is_array());
    }
}

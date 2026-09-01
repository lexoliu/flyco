//! Public execution-plane release artifacts.
//!
//! The source repository is private, so GitHub release URLs cannot bootstrap
//! an unauthenticated VM. Large binaries also cannot ride inside the Worker:
//! the free Worker bundle limit is smaller than one `flycod` build. They live
//! in the deployment's R2 bucket and this module exposes only the four exact
//! development-channel objects the installer needs.

use skyzen_services::{Storage, StorageError};

const ROOT: &str = "releases/dev";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Artifact {
    name: &'static str,
    content_type: &'static str,
}

const ARTIFACTS: [Artifact; 4] = [
    Artifact {
        name: "flycod-linux-x86_64",
        content_type: "application/octet-stream",
    },
    Artifact {
        name: "flycod-linux-x86_64.sha256",
        content_type: "text/plain; charset=utf-8",
    },
    Artifact {
        name: "flycod-linux-aarch64",
        content_type: "application/octet-stream",
    },
    Artifact {
        name: "flycod-linux-aarch64.sha256",
        content_type: "text/plain; charset=utf-8",
    },
];

/// One allowlisted release artifact read from object storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifact {
    /// Bytes returned to the machine installer.
    pub body: Vec<u8>,
    /// Fixed media type for this artifact kind.
    pub content_type: &'static str,
}

/// Reads an allowlisted development-channel artifact.
///
/// Unknown names never reach object storage. A known name whose object has
/// not been published is also absent, which makes a half-published release
/// fail as a 404 rather than serving unrelated bucket content.
///
/// # Errors
///
/// Returns [`StorageError`] if R2 cannot be read.
pub async fn get(storage: &Storage, name: &str) -> Result<Option<ReleaseArtifact>, StorageError> {
    let Some(artifact) = ARTIFACTS.iter().find(|artifact| artifact.name == name) else {
        return Ok(None);
    };
    let key = format!("{ROOT}/{}", artifact.name);
    Ok(storage.get(&key).await?.map(|object| ReleaseArtifact {
        body: object.body,
        content_type: artifact.content_type,
    }))
}

#[cfg(test)]
mod tests {
    use skyzen_services::Storage;

    use super::{ROOT, get};

    fn store() -> Storage {
        Storage::new(skyzen_test::mock::InMemoryStorage::new())
    }

    #[skyzen::test]
    async fn an_unknown_name_cannot_read_an_arbitrary_bucket_object() {
        let storage = store();
        storage
            .put("transcripts/private", b"secret".to_vec())
            .await
            .expect("seed");

        assert_eq!(
            get(&storage, "../transcripts/private").await.expect("read"),
            None
        );
    }

    #[skyzen::test]
    async fn a_published_checksum_is_served_with_its_fixed_media_type() {
        let storage = store();
        storage
            .put(
                &format!("{ROOT}/flycod-linux-x86_64.sha256"),
                b"digest  flycod-linux-x86_64\n".to_vec(),
            )
            .await
            .expect("seed");

        let artifact = get(&storage, "flycod-linux-x86_64.sha256")
            .await
            .expect("read")
            .expect("published");
        assert_eq!(artifact.content_type, "text/plain; charset=utf-8");
        assert_eq!(artifact.body, b"digest  flycod-linux-x86_64\n");
    }
}

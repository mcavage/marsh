//! Shell-template-only Docker reference normalization. Generic OCI job/Kit
//! identity parsing is deliberately unchanged.
use crate::SbxError;
use marsh_contracts::OciImage;

/// An immutable named shell template, canonicalized before authority checks.
///
/// Identity is the digest alone. Locally loaded templates also keep their
/// import tag: stock SBX resolves a local template only by tag, so creation
/// uses the tag and the caller verifies the created VM's image digest.
#[derive(Clone, Debug)]
pub struct ShellTemplateReference(OciImage, Option<String>);

impl PartialEq for ShellTemplateReference {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for ShellTemplateReference {}

impl ShellTemplateReference {
    /// Require a named digest. Bare image IDs are not shell-template authority.
    /// Docker's familiar Hub names and legacy index name denote the same repo;
    /// an optional tag alongside a digest does not change its immutable identity.
    ///
    /// # Errors
    /// Rejects mutable/bare/ambiguous template references before SDK effects.
    pub fn parse(value: &str) -> Result<Self, SbxError> {
        let image = OciImage::parse(value.to_owned())?;
        let (repository, digest) = image.as_str().rsplit_once('@').ok_or_else(|| {
            SbxError::HostGrantFence(
                "shell templates require a named repository@sha256 digest, not a bare image ID"
                    .into(),
            )
        })?;
        let (registry, path) = match repository.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first, rest)
            }
            _ => ("docker.io", repository),
        };
        let registry = match registry {
            "index.docker.io" => "docker.io",
            other => other,
        };
        let (parent, leaf) = path.rsplit_once('/').map_or(("", path), |(p, l)| (p, l));
        let (leaf, tag) = leaf
            .split_once(':')
            .map_or((leaf, None), |(name, tag)| (name, Some(tag)));
        if leaf.is_empty() || parent.contains(':') {
            return Err(SbxError::HostGrantFence(
                "ambiguous shell template repository".into(),
            ));
        }
        let path = if parent.is_empty() && registry == "docker.io" {
            format!("library/{leaf}")
        } else if parent.is_empty() {
            leaf.to_owned()
        } else {
            format!("{parent}/{leaf}")
        };
        let image = OciImage::parse(format!("{registry}/{path}@{digest}"))?;
        let tagged = tag.map(|tag| format!("{registry}/{path}:{tag}"));
        Ok(Self(image, tagged))
    }

    /// Reference passed to `sbx create --template`. Local templates require
    /// their import tag; the digest form is not resolvable from the local store.
    ///
    /// # Errors
    /// Rejects a local template reference that lacks its import tag.
    pub fn create_reference(&self) -> Result<&str, SbxError> {
        if !self.requires_local_authority() {
            return Ok(self.0.as_str());
        }
        self.1.as_deref().ok_or_else(|| {
            SbxError::HostGrantFence(
                "local shell templates require repository:tag@sha256 (re-run make dev or prepare-shell-image)"
                    .into(),
            )
        })
    }

    /// The optional tag written alongside the digest (`name:tag@sha256:...`).
    #[must_use]
    pub fn tag(&self) -> Option<&str> {
        self.1
            .as_deref()
            .and_then(|tagged| tagged.rsplit_once(':'))
            .map(|(_, tag)| tag)
    }

    /// The immutable `sha256:` digest this template must resolve to.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.0
            .as_str()
            .rsplit_once('@')
            .map_or("", |(_, digest)| digest)
    }

    #[must_use]
    pub fn image(&self) -> &OciImage {
        &self.0
    }

    #[must_use]
    pub fn requires_local_authority(&self) -> bool {
        [
            "docker.io/library/marsh-shell-local@",
            "docker.io/library/marsh-dev-shell@",
        ]
        .iter()
        .any(|prefix| self.0.as_str().starts_with(prefix))
    }
}

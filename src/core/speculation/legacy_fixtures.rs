// Test-only legacy metadata producer. It deliberately does not call the old identity resolver,
// initialize a bare store, create checkpoints, or recover any journal.
impl PlannedLegacyNamespace {
    pub(crate) fn admit(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<LegacyNamespace> {
        self.verify_admission(writer, physical)?;
        let root = super::namespace::create_metadata_directory(&self.metadata_root)?;
        self.ancestor.verify()?;
        let registry_path = self.metadata_root.join("registry.lock");
        if !registry_path.exists() {
            std::fs::write(&registry_path, b"fixture registry lock")?;
        }
        let registry_lock = checked_lock(&registry_path, LockMode::Exclusive, LOCK_TIMEOUT)?;
        let mut registry = read_registry(&self.metadata_root)?;
        let existing = self.registry_ids(&registry)?;
        ensure!(
            self.planned_ids.is_none() || self.planned_ids == existing,
            "Time Machine registry mapping changed after planning"
        );
        if existing.is_none() {
            let key = |path: &Path| path.to_str().unwrap().replace('\\', "/");
            let common = key(self.identity.canonical_common_git_dir.as_ref().unwrap());
            let git = key(self.identity.canonical_git_dir.as_ref().unwrap());
            let mint = |prefix: &str| -> Result<String> {
                let mut bytes = [0u8; 8];
                getrandom::getrandom(&mut bytes)
                    .map_err(|e| anyhow::anyhow!("fixture entropy: {e}"))?;
                Ok(format!("{prefix}-{:016x}", u64::from_be_bytes(bytes)))
            };
            let wt = WorktreeEntry {
                worktree_id: mint("wt")?,
                git_dir: git,
            };
            if let Some(repo) = registry
                .repos
                .iter_mut()
                .find(|r| r.common_git_dir == common)
            {
                repo.worktrees.push(wt);
            } else {
                registry.repos.push(RepoEntry {
                    repo_id: mint("repo")?,
                    root_commit: None,
                    common_git_dir: common,
                    worktrees: vec![wt],
                });
            }
        }
        registry.schema_version = 1;
        validate_registry(&registry)?;
        let bytes = serde_json::to_vec_pretty(&registry)?;
        ensure!(
            bytes.len() as u64 <= MAX_REGISTRY_BYTES,
            "fixture registry exceeds bound"
        );
        crate::core::persist::atomic_write(&self.metadata_root.join("registry.json"), &bytes)?;
        let (repo, wt) = self
            .registry_ids(&registry)?
            .context("fixture mapping missing")?;
        root.verify()?;
        drop(registry_lock);
        let repo_dir = self.metadata_root.join(repo);
        let namespace = repo_dir.join("worktrees").join(wt);
        super::namespace::create_metadata_directory(&namespace)?;
        for lock in [
            repo_dir.join("store.lock"),
            namespace.join("transaction.lock"),
        ] {
            if !lock.exists() {
                std::fs::write(lock, b"fixture existing lock")?;
            }
        }
        root.verify()?;
        // Bind the now-existing mapping, not a plan that predated the test-only producer.
        let mut plan = self.clone();
        plan.planned_ids = plan.registry_ids(&registry)?;
        plan.open_existing()?.context("fixture namespace missing")
    }
}

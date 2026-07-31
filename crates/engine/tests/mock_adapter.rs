//! Integration tests for MockVmAdapter.
//!
//! The mock adapter types live in `tests/common/mod.rs`.

mod common;

#[cfg(test)]
mod tests {
    use super::common::MockVmAdapter;
    use adapter_traits::{ExecOpts, FsSpec, Snapshot, UpperPolicy, VmAdapter, VmName, VmSpec};

    fn test_spec() -> VmSpec {
        VmSpec {
            name: VmName::new("test-vm").unwrap(),
            kernel: "/fake/vmlinux".into(),
            cmdline: None,
            boot_vcpus: 1,
            max_vcpus: Some(4),
            memory_mb: 256,
            max_memory_mb: Some(1024),
            initramfs: None,
            net: false,
            fs: None,
            backend_config: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_info() {
        let adapter = MockVmAdapter::new().with_state("Running").with_pid(42);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert_eq!(handle.pid(), 42);

        let info = handle.info().await.unwrap();
        assert_eq!(info.state, "Running");
    }

    #[tokio::test]
    async fn test_is_alive_and_shutdown() {
        let adapter = MockVmAdapter::new().with_alive(true);
        let spec = test_spec();
        let mut handle = adapter.create(&spec).await.unwrap();

        assert!(handle.is_alive());

        handle.shutdown().await.unwrap();
        assert!(!handle.is_alive());

        let info = handle.info().await.unwrap();
        assert_eq!(info.state, "ShutDown");
    }

    #[tokio::test]
    async fn test_exec_result() {
        let adapter = MockVmAdapter::new().with_exec("hello\n", "oops\n", 1);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        let result = handle
            .exec(&ExecOpts::new(vec!["echo".into(), "hello".into()], 10))
            .await
            .unwrap();
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "oops\n");
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn test_snapshot_not_supported() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert!(handle.snapshot().await.is_err());
    }

    #[tokio::test]
    async fn test_pause_resume_not_supported() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert!(handle.pause().await.is_err());
        assert!(handle.resume().await.is_err());
    }

    #[tokio::test]
    async fn test_restore_not_supported() {
        let adapter = MockVmAdapter::new();
        let snapshot = Snapshot {
            path: "/fake".into(),
        };
        let spec = test_spec();
        assert!(adapter.restore(&snapshot, &spec).await.is_err());
    }

    #[tokio::test]
    async fn test_attach_detach_fs() {
        let adapter = MockVmAdapter::new().with_fs_attached(false);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        let fs = FsSpec {
            layers: vec!["base".into()],
            upper: UpperPolicy::Ephemeral,
        };

        handle.attach_fs(&fs).await.unwrap();
        handle.detach_fs().await.unwrap();
    }

    #[tokio::test]
    async fn test_capabilities() {
        let adapter = MockVmAdapter::new()
            .with_capability("cpu_resize", true)
            .with_capability("memory_resize", true);
        let caps = adapter.capabilities();
        assert!(caps.cpu_resize);
        assert!(caps.memory_resize);
        // Defaults should be false
        assert!(!caps.snapshot);
        assert!(!caps.pause_resume);
    }

    #[tokio::test]
    async fn test_resize_noop() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        // Resize is a no-op success in the mock
        handle.resize(Some(2), Some(512)).await.unwrap();
    }

    #[tokio::test]
    async fn test_builder_chaining() {
        let adapter = MockVmAdapter::new()
            .with_state("Running")
            .with_pid(42)
            .with_exec("hello\n", "", 0)
            .with_alive(true)
            .with_fs_attached(true)
            .with_capability("cpu_resize", true);

        let spec = test_spec();
        let mut handle = adapter.create(&spec).await.unwrap();

        assert_eq!(handle.pid(), 42);
        assert!(handle.is_alive());

        let result = handle
            .exec(&ExecOpts::new(vec!["true".into()], 5))
            .await
            .unwrap();
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
    }
}

//! `confirm_durable` stands in front of every VM `sbx create`: while the
//! ownership map is published but not known durable (a failed directory
//! sync), no create is submitted. One test per create site, each against the
//! stateful stock model, with a control that the same open does create.
use super::stock_model::StockModel;
use super::*;
use crate::vm_ownership::FailDirectorySync;

/// Opens a VM through one create site of an adapter over `model` and `map`.
type Open = fn(&StockSbx) -> Result<(), SbxError>;

/// The create site runs once on a healthy map, and not at all once a write was
/// published but its directory sync failed (and keeps failing).
fn assert_no_create_on_an_undurable_map(open: Open) {
    let control = scratch();
    let healthy = StockModel::new();
    let adapter = StockSbx::new("sbx", Arc::clone(&healthy) as Arc<dyn CommandRunner>)
        .with_vm_ownership(control.path().join("healthy.json"))
        .unwrap();
    open(&adapter).unwrap();
    assert_eq!(healthy.count("create"), 1, "the control did not create");

    let model = StockModel::new();
    let adapter = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(control.path().join("undurable.json"))
        .unwrap();
    let fail = FailDirectorySync::in_dir(control.path());
    assert!(adapter.ownership.assign(VmPurpose::Kit, "other").is_err());
    let opened = open(&adapter);
    drop(fail);
    assert!(opened.is_err(), "{opened:?}");
    assert_eq!(model.count("create"), 0, "a create ran on an undurable map");
}

#[test]
fn the_shell_create_waits_for_a_durable_map() {
    assert_no_create_on_an_undurable_map(|adapter| {
        let shell = ShellFixture::new();
        adapter.ensure_shell_vm(&shell.spec).map(drop)
    });
}

/// `create_services_vm` is handed a name chosen earlier, so its gate is
/// exercised on its own: the name is assigned while the map is healthy, and the
/// map then goes undurable before the create.
#[test]
fn the_services_create_waits_for_a_durable_map() {
    let control = scratch();
    let shell = ShellFixture::new();
    let project = directory(shell.dir.path(), "project");
    let spec = ServicesVmSpec {
        project: project.clone(),
        image: shell.spec.image.clone(),
        shell_binary: shell.spec.shell_binary.clone(),
        disk_size: "2G".into(),
        pool_bytes: None,
    };
    let key = project.to_str().unwrap();
    let adapter_over = |model: &Arc<StockModel>, file: &str| {
        StockSbx::new("sbx", Arc::clone(model) as Arc<dyn CommandRunner>)
            .with_vm_ownership(control.path().join(file))
            .unwrap()
    };

    let healthy = StockModel::new();
    let adapter = adapter_over(&healthy, "healthy.json");
    let name = adapter.vm_name(VmPurpose::Services, key).unwrap();
    adapter.create_services_vm(&spec, &name).unwrap();
    assert_eq!(healthy.count("create"), 1, "the control did not create");

    let model = StockModel::new();
    let adapter = adapter_over(&model, "undurable.json");
    let name = adapter.vm_name(VmPurpose::Services, key).unwrap();
    let fail = FailDirectorySync::in_dir(control.path());
    assert!(adapter.ownership.assign(VmPurpose::Kit, "other").is_err());
    let created = adapter.create_services_vm(&spec, &name);
    drop(fail);
    assert!(created.is_err(), "{created:?}");
    assert_eq!(model.count("create"), 0, "a create ran on an undurable map");
}

#[test]
fn the_published_kit_create_waits_for_a_durable_map() {
    assert_no_create_on_an_undurable_map(|adapter| {
        let kit = KitFixture::published("marsh-kit-fixture");
        adapter.ensure_kit_vm(&kit.spec).map(drop)
    });
}

#[test]
fn the_local_kit_create_waits_for_a_durable_map() {
    assert_no_create_on_an_undurable_map(|adapter| {
        let kit = KitFixture::local("marsh-kit-local");
        adapter.ensure_kit_vm(&kit.spec).map(drop)
    });
}

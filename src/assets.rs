use std::path::{Path, PathBuf};

pub fn asset(name: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("PHONEBOX_ASSETS") {
        return PathBuf::from(dir).join(name);
    }
    if let Some(dir) = installed_dir() {
        return dir.join(name);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name)
}

fn installed_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("share/phonebox");
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::asset;

    #[test]
    fn cargo_run_reads_assets_from_the_source_tree() {
        let path = asset("widgets.json");
        assert!(path.ends_with("assets/widgets.json"));
        assert!(path.is_file());
    }
}

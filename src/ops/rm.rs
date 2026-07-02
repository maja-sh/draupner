use anyhow::Result;

pub fn run(_branch: &str, _force: bool) -> Result<()> {
    crate::ops::not_implemented("ws rm")
}

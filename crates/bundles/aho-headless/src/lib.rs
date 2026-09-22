//! `08` — the spine plus the headless surfaces: one-shot print/`--json`, the NDJSON rpc protocol the
//! SDKs and the daemon topology speak, the GitHub webhook listener (Locked until its credential
//! exists), and the Agent Client Protocol server editors drive over stdio (`aho acp`).
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use aho_cap::Composition;

pub const NAME: &str = "aho-headless";

pub fn headless() -> Composition {
    Composition::new(NAME)
        .include(aho_base::spine())
        .surface::<surface_print::SurfacePrint>()
        .surface::<surface_rpc::SurfaceRpc>()
        .surface::<surface_acp::SurfaceAcp>()
        .surface::<surface_webhook::SurfaceWebhook>()
}

#[cfg(test)]
mod tests {
    use aho_compose::{Inputs, load};
    use aho_config::Layer;
    use aho_log::CapState;

    #[test]
    fn surfaces_are_active_and_spine_defaults_carry_the_product_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut i = Inputs::new(dir.path(), dir.path().join("home"));
        if let Ok(p) = std::env::var("PATH") {
            i.env.insert("PATH".into(), p);
        }
        let p = load(&super::headless(), &i).unwrap();
        for id in ["surface-print", "surface-rpc", "surface-acp", "tool-fs"] {
            assert_eq!(p.plan.row(id).map(|r| r.state.clone()), Some(CapState::Active), "{id}");
        }
        let preset = &p.loadout.config["policy.preset"];
        assert_eq!(preset.winner.layer, Layer::Bundle(super::NAME.into()), "included defaults are attributed to this product");
    }
}

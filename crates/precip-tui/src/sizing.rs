//! The Sizing tab: the assumptions the user can change, and the result.

use crate::{App, TuiError};
use precip_engine::sizing::inputs::SizedInstance;
use precip_engine::sizing::profiles::BuiltinProfile;
use precip_engine::sizing::{Assumptions, Instance, Sizing, WorkloadDemand};

/// Sizing state: the profile, the assumptions the user can turn, the
/// assessed instances of the latest scan, and the result.
pub(crate) struct SizingView {
    pub profile: precip_types::Profile,
    pub assumptions: Assumptions,
    pub instances: Vec<SizedInstance>,
    pub result: Sizing,
    pub demand: Vec<WorkloadDemand>,
}

impl SizingView {
    pub(crate) fn compute(&mut self) {
        let inputs: Vec<Instance> =
            self.instances.iter().map(Into::into).collect();
        self.result = Sizing::new(&self.profile, &inputs, &self.assumptions);
        self.demand = WorkloadDemand::by_pair(&self.instances);
    }
}

impl App {
    /// The next built-in profile, saved as the workspace's choice.
    pub(crate) fn next_profile(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let Some(v) = &self.sizing else { return Ok(()) };
        let next = v
            .profile
            .name
            .parse::<BuiltinProfile>()
            .unwrap_or(BuiltinProfile::DEFAULT)
            .next();
        tokio::runtime::Handle::current()
            .block_on(store.set_sizing_profile(next.as_str()))?;
        self.refresh()?;
        if let Some(v) = &self.sizing {
            self.note(format!(
                "sizing with {}: {} GiB per sled, release {}",
                v.profile.name,
                v.profile.sled.dram_gib,
                v.profile.oxide_release
            ));
        }
        Ok(())
    }

    pub(crate) fn adjust_sizing(
        &mut self,
        change: impl FnOnce(&mut Assumptions),
    ) {
        if let Some(v) = self.sizing.as_mut() {
            change(&mut v.assumptions);
            v.compute();
            let a = v.assumptions;
            self.note(format!(
                "sizing assumptions: growth x{:.1}, threshold {:.0}%, N+{}",
                a.growth_factor,
                a.threshold * 100.0,
                a.headroom
            ));
        }
    }
}

//! Cached NVML context using nvml-wrapper.
use anyhow::{Context, Result};
/// Lazily initialized per sensor, reused across successful samples. Device
/// handles borrow the context only during a read, so failed devices can be
/// rediscovered safely after dropping the context.
#[derive(Default)]
pub struct NvmlSensor {
    context: Option<nvml_wrapper::Nvml>,
}
impl NvmlSensor {
    pub fn read(&mut self, bus: &str) -> Result<f64> {
        use nvml_wrapper::{Nvml, enum_wrappers::device::TemperatureSensor};
        let bus = if bus.matches(':').count() == 1 {
            format!("0000:{bus}")
        } else {
            bus.to_owned()
        };
        if self.context.is_none() {
            self.context = Some(Nvml::init().context("initializing NVML (libnvidia-ml.so.1)")?);
        }
        let result = (|| {
            let device = self
                .context
                .as_ref()
                .unwrap()
                .device_by_pci_bus_id(bus.as_str())
                .with_context(|| format!("NVML device lookup for {bus}"))?;
            device
                .temperature(TemperatureSensor::Gpu)
                .map(f64::from)
                .context("NVML temperature read failed")
        })();
        if result.is_err() {
            self.context = None;
        }
        result
    }
}

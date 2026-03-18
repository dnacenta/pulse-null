pub async fn pulse() -> Result<(), Box<dyn std::error::Error>> {
    crate::vigil::pulse::run().map_err(|e| e.into())
}

pub async fn collect(trigger: String) -> Result<(), Box<dyn std::error::Error>> {
    crate::vigil::collect::run(&trigger).map_err(|e| e.into())
}

pub async fn status(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    crate::vigil::status::run(json).map_err(|e| e.into())
}

pub async fn init() -> Result<(), Box<dyn std::error::Error>> {
    crate::vigil::init::run().map_err(|e| e.into())
}

pub struct Config {
    pub secret_name: String,

    pub schedules_table_name: String,
    pub installations_table_name: String,
    
    pub schedule_name_prefix: String,
}

impl Config {
    pub fn new(env: &str) -> Config {
        Config {
            secret_name: "on-call-support/secrets".to_string(),
            
            schedules_table_name: format!("on-call-support-schedules-{}", env),
            installations_table_name: format!("on-call-support-installations-{}", env),

            schedule_name_prefix: "on-call-support-dev_UpdateUserGroupSchedule_".to_string(),
        }
    }
}

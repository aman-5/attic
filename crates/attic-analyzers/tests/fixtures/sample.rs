extern crate serde as serde_crate;

pub mod tools {
    // NotReal AlsoFake fake()
    pub trait Named {}

    pub trait Worker: Named {
        fn run(&self);
    }

    pub struct Runner;

    pub union Data {
        pub int_value: i32,
        pub float_value: f32,
    }

    pub enum Mode {
        Fast,
        Slow,
    }

    pub type RunnerId = usize;
    pub const MAX_RETRIES: usize = 3;
    pub static DEFAULT_NAME: &str = "worker";

    macro_rules! log_value {
        ($value:expr) => {
            println!("{}", $value);
        };
    }

    impl Runner {
        pub fn status(&self) -> &'static str {
            "ok"
        }
    }

    impl Named for Runner {}

    impl Worker for Runner {
        fn run(&self) {
            helper();
            self.status();
            log_value!(DEFAULT_NAME);
        }
    }

    pub fn helper() {}
}

mod support;
use crate::tools::{helper, Runner, Worker};
use self::tools::RunnerId;

pub mod nested {
    use super::tools::{self, helper as parent_helper};

    pub fn boot() {
        parent_helper();
        tools::helper();
    }
}

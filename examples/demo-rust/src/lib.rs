//! Toy library used by the codegraph indexer demo.

pub mod greet {
    pub fn hello(name: &str) -> String {
        format!("hello, {}", name)
    }

    pub fn shout(name: &str) -> String {
        hello(name).to_uppercase()
    }
}

pub mod math {
    pub fn add(a: i64, b: i64) -> i64 {
        a + b
    }

    pub fn double(x: i64) -> i64 {
        add(x, x)
    }
}

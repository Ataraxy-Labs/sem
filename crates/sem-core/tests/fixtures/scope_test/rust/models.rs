pub struct Dog {
    pub name: String,
}

impl Dog {
    pub fn new(name: String) -> Dog {
        Dog { name }
    }

    pub fn speak(&self) -> &str {
        "woof"
    }

    pub fn validate(&self) -> bool {
        !self.name.is_empty()
    }
}

pub struct Cat {
    pub name: String,
}

impl Cat {
    pub fn new(name: String) -> Cat {
        Cat { name }
    }

    pub fn speak(&self) -> &str {
        "meow"
    }

    pub fn validate(&self) -> bool {
        !self.name.is_empty() && self.name.len() < 50
    }
}

pub struct Shelter {
    pub animals: Vec<String>,
}

impl Shelter {
    pub fn new() -> Shelter {
        Shelter { animals: Vec::new() }
    }

    pub fn add(&mut self, name: String) {
        self.animals.push(name);
    }

    pub fn count(&self) -> usize {
        self.animals.len()
    }
}

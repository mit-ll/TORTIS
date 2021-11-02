// Test for absence of validation mismatch ICE in #65394

const _: Vec<i32> = {
    let mut x = Vec::<i32>::new();
    let r = &mut x; //~ ERROR references in constants may only refer to immutable values
    let y = x;
    y
};

fn main() {}

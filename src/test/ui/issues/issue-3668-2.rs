fn f(x:isize) {
    static child: isize = x + 1;
    //~^ ERROR attempt to use a non-constant value in a constant
}

fn main() {}

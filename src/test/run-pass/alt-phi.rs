

tag thing { a; b; c; }

fn foo(it: block(int)) { it(10); }

fn main() {
    let x = true;
    alt a {
      a. { x = true; foo {|_i|} }
      b. { x = false; }
      c. { x = false; }
    }
}

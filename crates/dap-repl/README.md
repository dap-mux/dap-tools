# dap-repl

A small interactive console for a program paused at a breakpoint. Type an expression to see
its value, or a command to step and continue.

```
(dap) len(primes)
=> 7 : int
(dap) :next
⏸ stop (step) → is_prime @ line 43
```

It connects to a [dap-mux](https://github.com/dap-mux/dap-mux) session your editor is
already debugging. You don't launch the program or set breakpoints from here. You drive and
inspect wherever it is stopped.

```sh
dap-repl                # connect to 127.0.0.1:5679, the default
dap-repl 5680           # a different port
dap-repl host:port      # a different host and port
```

## Commands

Anything that does not start with a colon is evaluated as an expression in the selected
frame. A colon starts a command. An empty line repeats the last input, command or
expression, so you can keep pressing Enter to keep stepping. Type `:help` for the full list.

```
:c :continue   resume execution
:n :next       step over
:s :step       step into
:o :finish     step out
:pause         pause a running program
:up :down      move to the calling or called frame
:frame <n>     select a frame by number
:bt :where     print the call stack
```

Driving acts on the thread the program last stopped on. Multiple threads are not
orchestrated.

## Heads up: this drives and can change your program

dap-repl is hands-on, not a read-only window. It steps, continues, and pauses the program,
and the expressions you type run for real inside it. Typing `x = 5` or calling a function
with side effects will actually do so. The debug session is shared, so everything you do
shows up for your editor too. If you only want to look without any chance of touching
anything, use dap-observer instead.

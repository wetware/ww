@0xe70969fa5df97f4b;

interface Shell {
  eval @0 (text :Text) -> (result :Text, isError :Bool, output :Text, exitRequested :Bool);
  # Evaluate a Glia s-expression and return the result as text.
  # isError is true when the input fails to parse or evaluation errors.
  # Session state persists across evals (nREPL semantics): def sticks.
}

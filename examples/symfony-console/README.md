# symfony/console on phpun

The real `symfony/console` 6.4 LTS package (Composer-installed, unmodified)
running end-to-end on phpun — `ArgvInput` parsing, `InputDefinition`
binding, `Application`/`Command` dispatch, `DescriptorHelper`, and the
`SymfonyStyle` output formatter.

## Run

```bash
phpun examples/symfony-console/console.php app:greet World --yell -i 2 --no-ansi
```

Output is byte-identical to reference PHP 8.5 CLI. Verified against the
oracle for `app:greet`, `list`, `help`, `--help`, `--version`,
`completion`, and the missing-argument error path (exit code 1).

## What's vendored

`composer.json` requires `symfony/console ^6.4`. `vendor/` contains the
real package plus its dependencies (`symfony/string`,
`symfony/service-contracts`, `symfony/deprecation-contracts`,
`symfony/polyfill-intl-*`) and the generated Composer autoloader
(`vendor/composer/ClassLoader.php`).

## Engine semantics this exercises

- COW array separation for shared `Rc<PhpArray>` on `unset($copy[$k])`,
  `array_shift($copy)`, and mutating builtins passed `&$array` —
  `ArgvInput::parse()` and `InputDefinition::parseArgument()` depend on it.
- `use` import rules: a plain `use` is a *class* alias only; qualified
  names resolve their first segment through the class-alias table
  regardless of symbol kind (`match`/`instanceof` in `Descriptor`).
- `match (true) { $x instanceof T => ... }` dispatch.
- `preg_replace` replacement escaping (`\\` → one literal backslash) in
  `OutputFormatter::escape`.
- CLI `argv[0]`/`PHP_SELF`/`SCRIPT_NAME`/`SCRIPT_FILENAME` = path
  as-invoked, while `__FILE__` stays canonical.

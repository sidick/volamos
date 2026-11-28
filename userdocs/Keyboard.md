# Keyboard and Terminal Input

An interactive Amiga program, such as a text editor or a game that
switches its console to raw mode with `SetMode`, reads keys as the Amiga
console reports them. Cursor up is `$9B A`, F1 is `$9B 0~`, Help is
`$9B ?~`. A host terminal sends the same keys in its own forms: cursor up
is `ESC [ A`, F1 is `ESC O P`. volamos translates between the two, so
the program sees the keys an Amiga keyboard would give.

This page covers which keys are translated, how to give the Amiga keys a
PC keyboard lacks (Help above all) a host key of their own, and what
happens to typed characters. The output direction is covered under
[`--term-escapes`](CLI-Reference.md#-term-escapes-autoonoff) and
[`--term-charset`](CLI-Reference.md#-term-charset-autoonoff).

## When keys are translated

Keys are translated when `--term-escapes` is on and the program has put
its console in raw mode. Under the default, `auto`, that means stdin is a
terminal. In ordinary line-by-line (cooked) input, the host terminal's
own line editing has already used the keys it knows, and the line is
delivered as typed.

## The default key map

The Amiga bytes are those Kickstart 3.1's default keymap gives, measured
on the real ROM. The host side follows xterm's conventions, which most
terminals share, and each key is recognized in all the forms listed.

| Host key | Host terminal sends | Amiga key | Program reads | With Shift |
| --- | --- | --- | --- | --- |
| Cursor up | `ESC [ A`, `ESC O A` | cursor up | `$9B A` | `$9B T` |
| Cursor down | `ESC [ B`, `ESC O B` | cursor down | `$9B B` | `$9B S` |
| Cursor right | `ESC [ C`, `ESC O C` | cursor right | `$9B C` | `$9B SP @` |
| Cursor left | `ESC [ D`, `ESC O D` | cursor left | `$9B D` | `$9B SP A` |
| F1–F4 | `ESC O P`–`S`, `ESC [ 11~`–`14~` | F1–F4 | `$9B 0~`–`$9B 3~` | `$9B 10~`–`$9B 13~` |
| F5–F10 | `ESC [ 15~`, `17~`–`21~` | F5–F10 | `$9B 4~`–`$9B 9~` | `$9B 14~`–`$9B 19~` |
| F11 | `ESC [ 23~` | F11 | `$9B 20~` | `$9B 30~` |
| F12 | `ESC [ 24~` | F12 | `$9B 21~` | `$9B 31~` |
| Insert | `ESC [ 2~` | Insert | `$9B 40~` | `$9B 50~` |
| Delete | `ESC [ 3~` | Del | `$7F` | `$7F` |
| Home | `ESC [ H`, `ESC O H`, `ESC [ 1~`, `ESC [ 7~` | Home | `$9B 44~` | `$9B 54~` |
| End | `ESC [ F`, `ESC O F`, `ESC [ 4~`, `ESC [ 8~` | End | `$9B 45~` | `$9B 55~` |
| Page Up | `ESC [ 5~` | Page Up | `$9B 41~` | `$9B 51~` |
| Page Down | `ESC [ 6~` | Page Down | `$9B 42~` | `$9B 52~` |
| Backspace | `$7F` | Backspace | `$08` | `$08` |
| Shift+Tab | `ESC [ Z` | Shift+Tab | — | `$9B Z` |
| Esc | `ESC` | Esc | `$1B` | |

Insert, Page Up, Page Down, Home, End, F11 and F12 aren't on classic
Amiga keyboards, but Kickstart 3.1's keymap already defines them, so
software that knows them gets them.

Shift is the only modifier passed on. The terminal reports it with
xterm's modifier parameter: Shift+Up is `ESC [ 1;2 A`. A key with Ctrl
or Alt, F13 and above, and any sequence volamos doesn't recognize is
dropped, since no Amiga key would produce it.

A lone Esc is held for a moment, in case it starts a key's sequence, and
then delivered as `$1B`. Esc followed at once by an ordinary character,
which is how many terminals send Alt+key, arrives as both bytes.

## Mapping a key: `--key`

```sh
volamos --key F12=HELP myeditor
```

`--key HOSTKEY=AMIGAKEY` makes a host key type an Amiga key. Give it
more than once to map several keys; later settings override earlier
ones. Names are case-insensitive, and either side can have a `Shift+`
prefix:

```sh
volamos --key F12=HELP --key Shift+F12=BREAK myeditor
```

Mapping a key without `Shift+` also maps its Shift form to the Amiga
key with Shift, unless you map that separately. The setting is checked
when volamos starts, and a name it doesn't know is an error that lists
the valid ones.

**Host keys:** `Up`, `Down`, `Right`, `Left`, `Home`, `End`, `Insert`,
`Delete`, `PageUp`, `PageDown`, `Tab`, `Backspace`, `F1`–`F20`.
F13–F20 have no default, so they are free to map, if your keyboard or
terminal can send them.

**Amiga keys:** the NDK's `RAWKEY_` names without the prefix:

| Name | Key | Name | Key |
| --- | --- | --- | --- |
| `HELP` | Help (`$9B ?~`) | `ESC` | Esc |
| `F1`–`F10` | function keys | `TAB` | Tab |
| `F11`, `F12` | extended function keys | `SPACE` | Space |
| `CRSRUP`, `CRSRDOWN` | cursor up, down | `BACKSPACE` | Backspace |
| `CRSRLEFT`, `CRSRRIGHT` | cursor left, right | `DEL` | Del |
| `HOME`, `END` | Home, End | `RETURN` | Return |
| `PAGEUP`, `PAGEDOWN` | Page Up, Page Down | `ENTER` | keypad Enter |
| `INSERT` | Insert | `BREAK` | Break (`$9B 43~`) |

In a `.volamos` file, use one `KEY` line per mapping (see
[Configuration Files](Configuration.md)). Mappings from every config file
and the command line all apply, in precedence order, so a command-line
`--key` overrides a config file's mapping of the same key.

```ini
KEY=F12=HELP
```

### Why Help has no default

Help is the Amiga key that a PC keyboard lacks and Amiga software most
often wants. volamos doesn't pick a host key for it, because every
common one already stands for an Amiga key that Kickstart 3.1 defines.
F1 is F1, F12 is F12, and Insert is Insert. Choose the key you can spare:
`F12=HELP` suits programs that only know the classic ten function keys,
and `F13=HELP` suits a keyboard that has one.

## Typed characters

Amiga text is ISO-8859-1 (Latin-1). With `--term-charset` on (by
default, `auto`: stdin is a terminal and the locale is UTF-8), characters
typed as UTF-8 are converted. An `é` arrives as the single byte `$E9`,
and a character Latin-1 doesn't have, such as `€`, arrives as `?`. Bytes
that aren't UTF-8 pass through unchanged, so a terminal already set to
Latin-1 still works. This applies in raw and cooked mode alike.

A piped stdin isn't converted by default. `--term-charset on` converts
it too.

The program's arguments are converted the same way. Under `auto`, this
happens whenever the locale is UTF-8, because that is how your shell
encoded them, terminal or not. `volamos echoargs café` gives the program
`caf` followed by `$E9`. An argument that isn't valid UTF-8 is passed as
it is.

## Answers from the console

Some programs ask the console a question and read the answer from their
input:

- **Window size** (`CSI SP q`): volamos answers with the Amiga's window
  bounds report, `$9B 1;1;rows;columns SP r`, made from the terminal's
  current size. It answers only when stdin is a terminal.
- **Cursor position** (`CSI 6 n`): passed to the terminal, whose answer
  `ESC [ row;column R` arrives as the Amiga's `$9B row;column R`. This
  needs raw mode and `--term-escapes`. Otherwise the request is dropped,
  since the answer would be stuck in a cooked line or arrive in a form
  the program doesn't expect.

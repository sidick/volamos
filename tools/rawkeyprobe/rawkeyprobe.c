/* rawkeyprobe.c: records console.device RawKeyConvert's output, for
 * comparing volamos with real Kickstart (issue #425). One line per case:
 *   <label> <actual> <hex bytes written>
 * The buffer is prefilled with 0xEE so writes past "actual" show up.
 *
 * Build: see build.sh.
 * Run it from the Startup-Sequence of a real Kickstart 3.1 (before any
 * SetMap, so the ROM's default keymap is in force) with its output
 * redirected to a file; the result is
 * crates/volamos-core/tests/data/console-rawkeyconvert-ks31.txt.
 *
 * InputEvent chains (ie_NextEvent) are deliberately limited to the three
 * shapes below: real Kickstart 3.1 converts chains erratically and hangs
 * on some (a key event followed by an IECLASS_NULL event with code 0). */
#include <exec/types.h>
#include <exec/io.h>
#include <devices/inputevent.h>
#include <devices/conunit.h>
#include <proto/exec.h>
#include <proto/console.h>
#include <stdio.h>
#include <string.h>

struct Device *ConsoleDevice;

static UBYTE buf[64];

static void show(const char *label, LONG actual, int dumplen)
{
    int i;
    printf("%s %ld", label, (long)actual);
    for (i = 0; i < dumplen; i++)
        printf(" %02x", buf[i]);
    printf("\n");
    fflush(stdout);
}

static void key(struct InputEvent *ie, UBYTE cls, UWORD code, UWORD qual)
{
    memset(ie, 0, sizeof(*ie));
    ie->ie_Class = cls;
    ie->ie_Code = code;
    ie->ie_Qualifier = qual;
}

static LONG convert(struct InputEvent *ie, LONG len)
{
    memset(buf, 0xEE, sizeof(buf));
    return RawKeyConvert(ie, (STRPTR)buf, len, NULL);
}

int main(void)
{
    static const UWORD quals[] = {
        0,
        IEQUALIFIER_LSHIFT,
        IEQUALIFIER_RSHIFT,
        IEQUALIFIER_CAPSLOCK,
        IEQUALIFIER_CONTROL,
        IEQUALIFIER_LSHIFT | IEQUALIFIER_CAPSLOCK,
        IEQUALIFIER_LSHIFT | IEQUALIFIER_CONTROL,
        IEQUALIFIER_CAPSLOCK | IEQUALIFIER_CONTROL,
        IEQUALIFIER_LALT,
        IEQUALIFIER_LALT | IEQUALIFIER_LSHIFT,
        IEQUALIFIER_LCOMMAND,
        IEQUALIFIER_NUMERICPAD,
        IEQUALIFIER_REPEAT,
        IEQUALIFIER_RALT,
        IEQUALIFIER_RALT | IEQUALIFIER_LSHIFT,
        IEQUALIFIER_RCOMMAND,
        IEQUALIFIER_REPEAT | IEQUALIFIER_LSHIFT,
        IEQUALIFIER_REPEAT | IEQUALIFIER_CONTROL,
        IEQUALIFIER_REPEAT | IEQUALIFIER_CAPSLOCK,
        IEQUALIFIER_REPEAT | IEQUALIFIER_LSHIFT | IEQUALIFIER_CONTROL,
    };
    static const UWORD hicodes[] = { 0x0120, 0x01a0, 0x2120, 0xff20 };
    struct IOStdReq ior;
    struct InputEvent e1, e2;
    char label[32];
    unsigned q, code, dk, sh;
    LONG n;

    memset(&ior, 0, sizeof(ior));
    if (OpenDevice((STRPTR)"console.device", CONU_LIBRARY, (struct IORequest *)&ior, 0)) {
        printf("open failed %d\n", ior.io_Error);
        return 20;
    }
    ConsoleDevice = ior.io_Device;
    printf("open ok unit=%08lx\n", (unsigned long)ior.io_Unit);

    /* Every raw code under every qualifier set. */
    for (q = 0; q < sizeof(quals) / sizeof(quals[0]); q++) {
        for (code = 0; code < 0x80; code++) {
            key(&e1, IECLASS_RAWKEY, code, quals[q]);
            n = convert(&e1, sizeof(buf));
            sprintf(label, "k%02x/q%04x", code, quals[q]);
            show(label, n, n > 0 ? n : 0);
        }
    }

    /* The previous key a dead key (Alt+F/G/H/J/K on the usa keymap),
     * as ie_Prev1DownCode/Qual, then every raw code, unshifted and
     * shifted. */
    for (dk = 0x23; dk <= 0x27; dk++) {
        for (sh = 0; sh < 2; sh++) {
            for (code = 0; code < 0x80; code++) {
                key(&e1, IECLASS_RAWKEY, code, sh);
                e1.ie_position.ie_dead.ie_prev1DownCode = dk;
                e1.ie_position.ie_dead.ie_prev1DownQual = IEQUALIFIER_LALT;
                n = convert(&e1, sizeof(buf));
                sprintf(label, "d%02x/k%02x/q%04x", dk, code, sh);
                show(label, n, n > 0 ? n : 0);
            }
        }
    }

    /* Alt+F before 'e', varied: in ie_Prev2 only; with Right Alt; in both
     * Prev1 and Prev2; with Shift+Alt; and a non-dead Alt+A instead. */
    key(&e1, IECLASS_RAWKEY, 0x12, 0);
    e1.ie_position.ie_dead.ie_prev2DownCode = 0x23;
    e1.ie_position.ie_dead.ie_prev2DownQual = IEQUALIFIER_LALT;
    show("prev2-d23-e", convert(&e1, sizeof(buf)), 4);
    key(&e1, IECLASS_RAWKEY, 0x12, 0);
    e1.ie_position.ie_dead.ie_prev1DownCode = 0x23;
    e1.ie_position.ie_dead.ie_prev1DownQual = IEQUALIFIER_RALT;
    show("ralt-d23-e", convert(&e1, sizeof(buf)), 4);
    e1.ie_position.ie_dead.ie_prev1DownQual = IEQUALIFIER_LALT;
    e1.ie_position.ie_dead.ie_prev2DownCode = 0x23;
    e1.ie_position.ie_dead.ie_prev2DownQual = IEQUALIFIER_LALT;
    show("double-d23-e", convert(&e1, sizeof(buf)), 4);
    key(&e1, IECLASS_RAWKEY, 0x12, 0);
    e1.ie_position.ie_dead.ie_prev1DownCode = 0x23;
    e1.ie_position.ie_dead.ie_prev1DownQual = IEQUALIFIER_LALT | IEQUALIFIER_LSHIFT;
    show("shiftalt-d23-e", convert(&e1, sizeof(buf)), 4);
    key(&e1, IECLASS_RAWKEY, 0x12, 0);
    e1.ie_position.ie_dead.ie_prev1DownCode = 0x20;
    e1.ie_position.ie_dead.ie_prev1DownQual = IEQUALIFIER_LALT;
    show("alta-e", convert(&e1, sizeof(buf)), 4);

    /* ie_Code above 0xFF. */
    for (q = 0; q < sizeof(hicodes) / sizeof(hicodes[0]); q++) {
        key(&e1, IECLASS_RAWKEY, hicodes[q], 0);
        sprintf(label, "hi%04x", hicodes[q]);
        show(label, convert(&e1, sizeof(buf)), 4);
    }

    /* Does it walk ie_NextEvent? 'a' -> 'b'. */
    key(&e1, IECLASS_RAWKEY, 0x20, 0);
    key(&e2, IECLASS_RAWKEY, 0x35, 0);
    e1.ie_NextEvent = &e2;
    n = convert(&e1, sizeof(buf));
    show("chain-a-b", n, 4);

    /* Key-up first, then a key-down. */
    key(&e1, IECLASS_RAWKEY, 0x20 | IECODE_UP_PREFIX, 0);
    key(&e2, IECLASS_RAWKEY, 0x35, 0);
    e1.ie_NextEvent = &e2;
    n = convert(&e1, sizeof(buf));
    show("chain-up-b", n, 4);

    /* Non-key class first, then a key-down. */
    key(&e1, IECLASS_NULL, 0x20, 0);
    key(&e2, IECLASS_RAWKEY, 0x35, 0);
    e1.ie_NextEvent = &e2;
    n = convert(&e1, sizeof(buf));
    show("chain-null-b", n, 4);

    /* Single key-up, single non-key event. */
    key(&e1, IECLASS_RAWKEY, 0x20 | IECODE_UP_PREFIX, 0);
    show("up-a", convert(&e1, sizeof(buf)), 4);
    key(&e1, IECLASS_NULL, 0x20, 0);
    show("null-a", convert(&e1, sizeof(buf)), 4);
    key(&e1, IECLASS_RAWKEY, 0x20, 0);
    e1.ie_SubClass = 5;
    show("subclass-a", convert(&e1, sizeof(buf)), 4);

    /* Buffer capacity: 0 for one char; 1 and 2 for F1's 3-byte CSI "0~". */
    key(&e1, IECLASS_RAWKEY, 0x20, 0);
    show("len0-a", convert(&e1, 0), 4);
    show("len1-a", convert(&e1, 1), 4);
    key(&e1, IECLASS_RAWKEY, 0x50, 0);
    show("len1-f1", convert(&e1, 1), 4);
    show("len2-f1", convert(&e1, 2), 4);
    show("len3-f1", convert(&e1, 3), 4);

    /* Chain overflow: 'a' then 'b' into one byte. */
    key(&e1, IECLASS_RAWKEY, 0x20, 0);
    key(&e2, IECLASS_RAWKEY, 0x35, 0);
    e1.ie_NextEvent = &e2;
    show("len1-chain-a-b", convert(&e1, 1), 4);

    CloseDevice((struct IORequest *)&ior);
    printf("done\n");
    return 0;
}

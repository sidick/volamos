/* icon.library behaviour probe: prints what the library does in the cases
 * volamos had to guess (issue #313, "Phase 0"). Run the same binary on a
 * real Kickstart (Copperline) and under volamos and diff the output.
 *
 * Usage: iconprobe <dir>   -- a writable directory (with trailing : or /)
 * Everything the probe creates lives under <dir>probe/. Output goes to
 * stdout. Build: see build.sh.
 */
#include <exec/types.h>
#include <dos/dos.h>
#include <workbench/workbench.h>
#include <intuition/intuition.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/icon.h>
#include <stdio.h>
#include <string.h>

struct Library *IconBase;

static char dir[256];

static void path(char *out, const char *leaf)
{
    strcpy(out, dir);
    AddPart(out, leaf, 256);
}

static void put_file(const char *leaf, const void *data, LONG len)
{
    char p[256];
    BPTR f;
    path(p, leaf);
    f = Open(p, MODE_NEWFILE);
    if (f) {
        if (len) Write(f, (APTR)data, len);
        Close(f);
    }
}

static void describe(const char *label, struct DiskObject *o)
{
    if (!o) {
        printf("%-34s NULL ioerr=%ld\n", label, (long)IoErr());
        return;
    }
    printf("%-34s obj magic=%04x ver=%d type=%d stack=%ld x=%ld y=%ld tool=%s tt=%s toolwin=%s rev=%ld\n",
           label, o->do_Magic, (int)o->do_Version, o->do_Type, (long)o->do_StackSize,
           (long)o->do_CurrentX, (long)o->do_CurrentY,
           o->do_DefaultTool ? (char *)o->do_DefaultTool : "-",
           o->do_ToolTypes ? "yes" : "no", o->do_ToolWindow ? "yes" : "no",
           (long)((ULONG)o->do_Gadget.UserData & 0xff));
    printf("%-34s   gadget l=%d t=%d w=%d h=%d flags=%04x act=%04x type=%04x id=%d render=%s select=%s\n",
           "", o->do_Gadget.LeftEdge, o->do_Gadget.TopEdge, o->do_Gadget.Width,
           o->do_Gadget.Height, o->do_Gadget.Flags, o->do_Gadget.Activation,
           o->do_Gadget.GadgetType, o->do_Gadget.GadgetID,
           o->do_Gadget.GadgetRender ? "yes" : "no", o->do_Gadget.SelectRender ? "yes" : "no");
    if (o->do_Gadget.GadgetRender) {
        struct Image *im = (struct Image *)o->do_Gadget.GadgetRender;
        printf("%-34s   render img l=%d t=%d w=%d h=%d depth=%d pick=%d onoff=%d data=%s\n", "",
               im->LeftEdge, im->TopEdge, im->Width, im->Height, im->Depth, im->PlanePick,
               im->PlaneOnOff, im->ImageData ? "yes" : "no");
    }
    if (o->do_Gadget.SelectRender) {
        struct Image *im = (struct Image *)o->do_Gadget.SelectRender;
        printf("%-34s   select img l=%d t=%d w=%d h=%d depth=%d pick=%d onoff=%d data=%s\n", "",
               im->LeftEdge, im->TopEdge, im->Width, im->Height, im->Depth, im->PlanePick,
               im->PlaneOnOff, im->ImageData ? "yes" : "no");
    }
    if (o->do_DrawerData) {
        struct DrawerData *d = o->do_DrawerData;
        struct NewWindow *w = &d->dd_NewWindow;
        printf("%-34s   drawer win=%d,%d %dx%d cur=%ld,%ld flags=%lx viewmodes=%d\n", "",
               w->LeftEdge, w->TopEdge, w->Width, w->Height, (long)d->dd_CurrentX,
               (long)d->dd_CurrentY, (long)d->dd_Flags, (int)d->dd_ViewModes);
        printf("%-34s   newwindow pens=%d,%d idcmp=%lx flags=%lx first=%lx check=%lx title=%lx screen=%lx bitmap=%lx min=%d,%d max=%u,%u type=%u\n",
               "", w->DetailPen, w->BlockPen, (long)w->IDCMPFlags, (long)w->Flags,
               (long)w->FirstGadget, (long)w->CheckMark, (long)w->Title, (long)w->Screen,
               (long)w->BitMap, w->MinWidth, w->MinHeight, w->MaxWidth, w->MaxHeight, w->Type);
    } else
        printf("%-34s   drawer none\n", "");
    FreeDiskObject(o);
}

int main(int argc, char **argv)
{
    char p[256], buf[64];
    struct DiskObject *o;
    BPTR lock;
    int i;

    if (argc < 2) {
        printf("usage: iconprobe <dir>\n");
        return 20;
    }
    IconBase = OpenLibrary("icon.library", 0);
    if (!IconBase) {
        printf("no icon.library\n");
        return 20;
    }
    printf("icon.library %d.%d\n", IconBase->lib_Version, IconBase->lib_Revision);

    strcpy(dir, argv[1]);
    strcat(dir, "probe");
    lock = CreateDir(dir);
    if (lock) UnLock(lock);

    /* 1. Default icons with no ENV:Sys/def_*.info. */
    for (i = 1; i <= 8; i++) {
        sprintf(buf, "GetDefDiskObject(%d)", i);
        SetIoErr(777);
        describe(buf, GetDefDiskObject(i));
    }
    SetIoErr(777);
    describe("GetDefDiskObject(0)", GetDefDiskObject(0));
    SetIoErr(777);
    describe("GetDefDiskObject(99)", GetDefDiskObject(99));

    /* 2. GetDiskObject(NULL). */
    SetIoErr(777);
    describe("GetDiskObject(NULL)", GetDiskObject(NULL));

    /* 3. Missing, junk, short and directory icon files. */
    path(p, "none");
    SetIoErr(777);
    describe("GetDiskObject(missing)", GetDiskObject(p));
    put_file("junk.info", "this is not an icon file at all, not one bit", 44);
    path(p, "junk");
    SetIoErr(777);
    describe("GetDiskObject(junk)", GetDiskObject(p));
    put_file("empty.info", "", 0);
    path(p, "empty");
    SetIoErr(777);
    describe("GetDiskObject(empty)", GetDiskObject(p));
    put_file("magic.info", "\xE3\x10\x00\x01", 4);
    path(p, "magic");
    SetIoErr(777);
    describe("GetDiskObject(magic only)", GetDiskObject(p));
    path(p, "dir.info");
    lock = CreateDir(p);
    if (lock) UnLock(lock);
    path(p, "dir");
    SetIoErr(777);
    describe("GetDiskObject(dir.info is a dir)", GetDiskObject(p));

    /* 4. GetDiskObjectNew on things with no icon. */
    put_file("text", "plain text", 10);
    path(p, "text");
    SetIoErr(777);
    describe("GetDiskObjectNew(text file)", GetDiskObjectNew(p));
    put_file("prog", "\x00\x00\x03\xF3\x00\x00\x00\x00", 8);
    path(p, "prog");
    SetIoErr(777);
    describe("GetDiskObjectNew(hunk file)", GetDiskObjectNew(p));
    path(p, "adir");
    lock = CreateDir(p);
    if (lock) UnLock(lock);
    SetIoErr(777);
    describe("GetDiskObjectNew(directory)", GetDiskObjectNew(p));
    path(p, "nothere");
    SetIoErr(777);
    describe("GetDiskObjectNew(missing)", GetDiskObjectNew(p));
    SetIoErr(777);
    describe("GetDiskObjectNew(volume)", GetDiskObjectNew(argv[1]));
    SetIoErr(777);
    describe("GetDiskObject(volume)", GetDiskObject(argv[1]));

    /* 4b. GetDiskObjectNew's type test: protection bits. */
    path(p, "text");
    SetProtection(p, FIBF_EXECUTE);
    SetIoErr(777);
    describe("GetDiskObjectNew(text, e denied)", GetDiskObjectNew(p));
    path(p, "prog");
    SetProtection(p, FIBF_EXECUTE);
    SetIoErr(777);
    describe("GetDiskObjectNew(hunk, e denied)", GetDiskObjectNew(p));
    SetProtection(p, 0);

    /* 5. Write a default and a plain icon (from the default, so there is
     * a valid DiskObject), then read it back. */
    o = GetDefDiskObject(WBPROJECT);
    if (!o) o = GetDiskObject(NULL);
    if (o) {
        static UBYTE *tts[] = {(UBYTE *)"A=1", (UBYTE *)"FLAG", (UBYTE *)"B=x|y", NULL};
        BOOL ok;
        o->do_Type = WBPROJECT;
        o->do_ToolTypes = tts;
        o->do_DefaultTool = (STRPTR)"SYS:Tools/Edit";
        o->do_StackSize = 12345;
        path(p, "wrote");
        SetIoErr(777);
        ok = PutDiskObject(p, o);
        printf("%-34s %s ioerr=%ld\n", "PutDiskObject(ok)", ok ? "TRUE" : "FALSE", (long)IoErr());
        path(p, "nodir/x");
        SetIoErr(777);
        ok = PutDiskObject(p, o);
        printf("%-34s %s ioerr=%ld\n", "PutDiskObject(missing dir)", ok ? "TRUE" : "FALSE", (long)IoErr());
        SetIoErr(777);
        ok = PutDiskObject(argv[1], o);
        printf("%-34s %s ioerr=%ld\n", "PutDiskObject(volume)", ok ? "TRUE" : "FALSE", (long)IoErr());
        strcpy(p, argv[1]);
        strcat(p, "Disk.info");
        lock = Lock(p, SHARED_LOCK);
        printf("%-34s %s\n", "volume Disk.info exists", lock ? "yes" : "no");
        if (lock) UnLock(lock);
        strcpy(p, argv[1]);
        strcat(p, ".info");
        lock = Lock(p, SHARED_LOCK);
        printf("%-34s %s\n", "volume .info exists", lock ? "yes" : "no");
        if (lock) UnLock(lock);
        SetIoErr(777);
        describe("GetDiskObject(volume, put)", GetDiskObject(argv[1]));
        path(p, "wrote.info");
        lock = Lock(p, SHARED_LOCK);
        printf("%-34s %s\n", "wrote.info exists", lock ? "yes" : "no");
        if (lock) {
            struct FileInfoBlock *fib = AllocDosObject(DOS_FIB, NULL);
            if (fib && Examine(lock, fib)) printf("%-34s %ld bytes\n", "wrote.info size", (long)fib->fib_Size);
            if (fib) FreeDosObject(DOS_FIB, fib);
            UnLock(lock);
        }
        o->do_DefaultTool = NULL;
        o->do_ToolTypes = NULL;
        FreeDiskObject(o);
        path(p, "wrote");
        SetIoErr(777);
        o = GetDiskObject(p);
        describe("GetDiskObject(wrote)", o);
    }

    /* 6. FindToolType / MatchToolValue edge cases. */
    {
        static UBYTE *tts[] = {(UBYTE *)"FLAG", (UBYTE *)"A=1", (UBYTE *)"E=", (UBYTE *)"flagged=2", NULL};
        UBYTE *v;
        v = FindToolType(tts, (STRPTR)"FLAG");
        printf("%-34s %s\n", "FindToolType(FLAG bare)", v ? (*v ? (char *)v : "<empty>") : "NULL");
        v = FindToolType(tts, (STRPTR)"flag");
        printf("%-34s %s\n", "FindToolType(flag, case)", v ? (*v ? (char *)v : "<empty>") : "NULL");
        v = FindToolType(tts, (STRPTR)"E");
        printf("%-34s %s\n", "FindToolType(E=)", v ? (*v ? (char *)v : "<empty>") : "NULL");
        v = FindToolType(tts, (STRPTR)"FLAGGED");
        printf("%-34s %s\n", "FindToolType(FLAGGED)", v ? (char *)v : "NULL");
        v = FindToolType(tts, (STRPTR)"FLA");
        printf("%-34s %s\n", "FindToolType(FLA prefix)", v ? (char *)v : "NULL");
    }

    /* 6b. BumpRevision (all versions): prefix case and numbering. */
    {
        static const char *names[] = {"foo", "copy_of_foo", "Copy_of_foo", "copy_2_of_foo",
                                      "Copy_2_of_foo", "COPY_OF_foo", "copy_0_of_foo",
                                      "012345678901234567890123456789", "copy_x_of_foo", "copy foo", "copy_", "copy__of_foo"};
        int n;
        for (n = 0; n < 11; n++) {
            char out[64];
            memset(out, '#', sizeof out);
            out[63] = 0;
            BumpRevision((STRPTR)out, (STRPTR)names[n]);
            printf("BumpRevision(\"%s\") = \"%s\"\n", names[n], out);
        }
    }

    /* 7. BumpRevisionLength boundaries (V45+; skipped if absent). */
    if (IconBase->lib_Version >= 45) {
        static const LONG lens[] = {0, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 30, 31, 107};
        static const char *names[] = {"foo", "copy_99_of_foo", "", "Copy_of_foo", "copy_0_of_foo", "copy_x_of_foo", "copy foo", "copy_1_of_foo"};
        int n, k;
        for (n = 0; n < 8; n++)
            for (k = 0; k < (int)(sizeof lens / sizeof lens[0]); k++) {
                char out[200];
                STRPTR r;
                memset(out, '#', sizeof out);
                out[199] = 0;
                r = BumpRevisionLength((STRPTR)out, (STRPTR)names[n], lens[k]);
                printf("BumpRevisionLength(\"%s\",%ld) = %s%s%s\n", names[n], (long)lens[k],
                       r ? "\"" : "NULL", r ? out : "", r ? "\"" : "");
            }
    } else
        printf("BumpRevisionLength: library too old\n");

    CloseLibrary(IconBase);
    return 0;
}

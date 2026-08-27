/*
 * ei-inject - inject key events into a cosmic-comp instance over libei.
 *
 * cosmic-comp does NOT listen on an EIS socket and does not honour
 * $LIBEI_SOCKET. The only way in is the D-Bus interface it exports on the
 * session bus:
 *
 *   bus name : com.system76.CosmicComp
 *   object   : /com/system76/CosmicComp/Ei
 *   interface: com.system76.CosmicComp.Ei
 *   method   : GetSenderSocket(u device_types) -> h fd
 *
 * device_types is the XDG RemoteDesktop DeviceType bitmask
 * (1 = keyboard, 2 = pointer, 4 = touchscreen). The returned fd is one end of
 * a socketpair whose other end the compositor feeds to its EIS context, so it
 * is used with ei_setup_backend_fd() rather than a socket path.
 *
 * The caller must own org.freedesktop.impl.portal.desktop.cosmic or
 * com.system76.CosmicOSK on that bus, unless the compositor was started with
 * COSMIC_ENFORCE_DBUS_OWNERS=0 (which is what nested-comp.sh does). This tool
 * requests com.system76.CosmicOSK anyway so it works either way.
 *
 * Keys injected this way go through cosmic-comp's full input pipeline
 * (shortcut filter -> input-method keyboard grab), unlike virtual-keyboard-v1
 * keys, which bypass it. That is the whole point of using libei for the tests.
 *
 * Build:  scripts/im-harness/build-ei-inject.sh
 * Usage:  ei-inject --bus unix:path=/tmp/im-harness-1000/runtime/dbus \
 *                   type "hello" key 28 sleep 200 hold 1000
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <libei.h>
#include <systemd/sd-bus.h>

/* evdev key codes we need; from linux/input-event-codes.h */
#define KEY_LEFTSHIFT 42

struct ascii_key {
    char ch;
    uint32_t code;
    bool shift;
};

/* US layout, which is the nested compositor's default keymap. */
static const struct ascii_key ascii_map[] = {
    {'a',30,0},{'b',48,0},{'c',46,0},{'d',32,0},{'e',18,0},{'f',33,0},{'g',34,0},
    {'h',35,0},{'i',23,0},{'j',36,0},{'k',37,0},{'l',38,0},{'m',50,0},{'n',49,0},
    {'o',24,0},{'p',25,0},{'q',16,0},{'r',19,0},{'s',31,0},{'t',20,0},{'u',22,0},
    {'v',47,0},{'w',17,0},{'x',45,0},{'y',21,0},{'z',44,0},
    {'A',30,1},{'B',48,1},{'C',46,1},{'D',32,1},{'E',18,1},{'F',33,1},{'G',34,1},
    {'H',35,1},{'I',23,1},{'J',36,1},{'K',37,1},{'L',38,1},{'M',50,1},{'N',49,1},
    {'O',24,1},{'P',25,1},{'Q',16,1},{'R',19,1},{'S',31,1},{'T',20,1},{'U',22,1},
    {'V',47,1},{'W',17,1},{'X',45,1},{'Y',21,1},{'Z',44,1},
    {'1',2,0},{'2',3,0},{'3',4,0},{'4',5,0},{'5',6,0},{'6',7,0},{'7',8,0},
    {'8',9,0},{'9',10,0},{'0',11,0},
    {'!',2,1},{'@',3,1},{'#',4,1},{'$',5,1},{'%',6,1},{'^',7,1},{'&',8,1},
    {'*',9,1},{'(',10,1},{')',11,1},
    {'-',12,0},{'_',12,1},{'=',13,0},{'+',13,1},
    {'[',26,0},{'{',26,1},{']',27,0},{'}',27,1},
    {';',39,0},{':',39,1},{'\'',40,0},{'"',40,1},
    {'`',41,0},{'~',41,1},{'\\',43,0},{'|',43,1},
    {',',51,0},{'<',51,1},{'.',52,0},{'>',52,1},{'/',53,0},{'?',53,1},
    {' ',57,0},{'\t',15,0},{'\n',28,0},
};

static struct ei *ei_ctx;
static struct ei_device *kbd;
static struct ei_device *textdev;
static bool kbd_ready;
static bool text_ready;
static bool disconnected;
static uint32_t sequence;
static bool verbose;

static void vlog(const char *fmt, ...)
{
    if (!verbose) return;
    va_list ap; va_start(ap, fmt);
    fprintf(stderr, "ei-inject: "); vfprintf(stderr, fmt, ap); fputc('\n', stderr);
    va_end(ap);
}

static int get_socket_from_dbus(const char *address, uint32_t device_types, bool claim_name)
{
    sd_bus *bus = NULL;
    sd_bus_error err = SD_BUS_ERROR_NULL;
    sd_bus_message *reply = NULL;
    int fd = -1, r;

    r = sd_bus_new(&bus);
    if (r < 0) { fprintf(stderr, "sd_bus_new: %s\n", strerror(-r)); return -1; }
    r = sd_bus_set_address(bus, address);
    if (r < 0) { fprintf(stderr, "sd_bus_set_address(%s): %s\n", address, strerror(-r)); goto out; }
    sd_bus_set_bus_client(bus, 1);
    r = sd_bus_start(bus);
    if (r < 0) { fprintf(stderr, "sd_bus_start: %s\n", strerror(-r)); goto out; }

    if (claim_name) {
        /* Satisfy cosmic-comp's ALLOWED_NAMES check without needing
         * COSMIC_ENFORCE_DBUS_OWNERS=0. Harmless if it fails. */
        r = sd_bus_request_name(bus, "com.system76.CosmicOSK", 0);
        if (r < 0)
            fprintf(stderr, "ei-inject: could not own com.system76.CosmicOSK: %s\n", strerror(-r));
    }

    r = sd_bus_call_method(bus, "com.system76.CosmicComp", "/com/system76/CosmicComp/Ei",
                           "com.system76.CosmicComp.Ei", "GetSenderSocket",
                           &err, &reply, "u", device_types);
    if (r < 0) {
        fprintf(stderr, "GetSenderSocket failed: %s: %s\n",
                err.name ? err.name : "?", err.message ? err.message : strerror(-r));
        goto out;
    }
    r = sd_bus_message_read(reply, "h", &fd);
    if (r < 0) { fprintf(stderr, "reply had no fd: %s\n", strerror(-r)); fd = -1; goto out; }
    fd = dup(fd); /* the message owns the original */
    if (fd < 0) perror("dup");
out:
    sd_bus_error_free(&err);
    sd_bus_message_unref(reply);
    sd_bus_unref(bus);
    return fd;
}

static void handle_event(struct ei_event *e)
{
    switch (ei_event_get_type(e)) {
    case EI_EVENT_CONNECT:
        vlog("connected");
        break;
    case EI_EVENT_DISCONNECT:
        vlog("disconnected by server");
        disconnected = true;
        break;
    case EI_EVENT_SEAT_ADDED: {
        struct ei_seat *seat = ei_event_get_seat(e);
        vlog("seat added: %s", ei_seat_get_name(seat));
        if (ei_seat_has_capability(seat, EI_DEVICE_CAP_KEYBOARD) &&
            ei_seat_has_capability(seat, EI_DEVICE_CAP_TEXT))
            ei_seat_bind_capabilities(seat, EI_DEVICE_CAP_KEYBOARD, EI_DEVICE_CAP_TEXT, NULL);
        else if (ei_seat_has_capability(seat, EI_DEVICE_CAP_KEYBOARD))
            ei_seat_bind_capabilities(seat, EI_DEVICE_CAP_KEYBOARD, NULL);
        break;
    }
    case EI_EVENT_DEVICE_ADDED: {
        struct ei_device *d = ei_event_get_device(e);
        if (!kbd && ei_device_has_capability(d, EI_DEVICE_CAP_KEYBOARD)) {
            kbd = ei_device_ref(d);
            vlog("keyboard device: %s", ei_device_get_name(d));
        } else if (!textdev && ei_device_has_capability(d, EI_DEVICE_CAP_TEXT)) {
            textdev = ei_device_ref(d);
            vlog("text device: %s", ei_device_get_name(d));
        }
        break;
    }
    case EI_EVENT_DEVICE_RESUMED: {
        struct ei_device *d = ei_event_get_device(e);
        ei_device_start_emulating(d, ++sequence);
        if (d == kbd) { kbd_ready = true; vlog("keyboard resumed"); }
        if (d == textdev) { text_ready = true; vlog("text device resumed"); }
        break;
    }
    case EI_EVENT_DEVICE_PAUSED: {
        struct ei_device *d = ei_event_get_device(e);
        if (d == kbd) kbd_ready = false;
        if (d == textdev) text_ready = false;
        vlog("device paused");
        break;
    }
    case EI_EVENT_DEVICE_REMOVED: {
        struct ei_device *d = ei_event_get_device(e);
        if (d == kbd) { ei_device_unref(kbd); kbd = NULL; kbd_ready = false; }
        if (d == textdev) { ei_device_unref(textdev); textdev = NULL; text_ready = false; }
        break;
    }
    default:
        break;
    }
}

static int64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static bool pump_once(int timeout_ms)
{
    struct pollfd pfd = { .fd = ei_get_fd(ei_ctx), .events = POLLIN };
    int r = poll(&pfd, 1, timeout_ms);
    if (r < 0 && errno != EINTR) { perror("poll"); return false; }
    ei_dispatch(ei_ctx);
    struct ei_event *e;
    while ((e = ei_get_event(ei_ctx))) {
        handle_event(e);
        ei_event_unref(e);
    }
    return !disconnected;
}

/* Dispatch events for the full timeout_ms (poll returns early on traffic, so
 * loop against a deadline - otherwise 'hold' would not actually hold). */
static bool pump(int timeout_ms)
{
    int64_t deadline = now_ms() + timeout_ms;
    if (!pump_once(timeout_ms > 0 ? timeout_ms : 0)) return false;
    while (timeout_ms > 0) {
        int64_t left = deadline - now_ms();
        if (left <= 0) break;
        if (!pump_once((int)left)) return false;
    }
    return !disconnected;
}

static bool wait_ready(int timeout_ms)
{
    int waited = 0;
    while (!kbd_ready && waited < timeout_ms) {
        if (!pump(50)) return false;
        waited += 50;
    }
    return kbd_ready;
}

static void key(uint32_t code, bool press)
{
    if (!kbd_ready) return;
    ei_device_keyboard_key(kbd, code, press);
    ei_device_frame(kbd, ei_now(ei_ctx));
    pump(0);
}

static void tap(uint32_t code, int hold_ms)
{
    key(code, true);
    pump(hold_ms);
    key(code, false);
    pump(10);
}

static void type_string(const char *s, int per_key_ms)
{
    for (const char *p = s; *p; p++) {
        const struct ascii_key *m = NULL;
        for (size_t i = 0; i < sizeof(ascii_map)/sizeof(ascii_map[0]); i++)
            if (ascii_map[i].ch == *p) { m = &ascii_map[i]; break; }
        if (!m) { fprintf(stderr, "ei-inject: no keycode for '%c', skipped\n", *p); continue; }
        if (m->shift) key(KEY_LEFTSHIFT, true);
        tap(m->code, per_key_ms / 2);
        if (m->shift) key(KEY_LEFTSHIFT, false);
        pump(per_key_ms);
    }
}

static void usage(void)
{
    fprintf(stderr,
"usage: ei-inject [--bus ADDRESS] [--delay MS] [--verbose] ACTION...\n"
"\n"
"  --bus ADDRESS   D-Bus session address of the NESTED compositor\n"
"                  (default $DBUS_SESSION_BUS_ADDRESS)\n"
"  --delay MS      delay between keys for 'type' (default 40)\n"
"  --no-claim-name do not request com.system76.CosmicOSK\n"
"\n"
"actions:\n"
"  key CODE        press+release evdev keycode (e.g. 28 = Return)\n"
"  down CODE       press evdev keycode\n"
"  up CODE         release evdev keycode\n"
"  type STRING     type an ASCII string on a US keymap\n"
"  utf8 STRING     send text through the ei_text device (cosmic extension)\n"
"  keysym HEX      send an X keysym through the ei_text device\n"
"  sleep MS        wait, dispatching events\n"
"  hold MS         keep the EI connection open (the compositor treats an\n"
"                  active EI keyboard connection as an input method when no\n"
"                  real IME is bound, which is what makes text-input-v3\n"
"                  clients get their 'enter' event)\n");
}

int main(int argc, char **argv)
{
    const char *address = getenv("DBUS_SESSION_BUS_ADDRESS");
    int delay = 40;
    bool claim_name = true;
    int i = 1;

    for (; i < argc; i++) {
        if (!strcmp(argv[i], "--bus") && i + 1 < argc) address = argv[++i];
        else if (!strcmp(argv[i], "--delay") && i + 1 < argc) delay = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--verbose") || !strcmp(argv[i], "-v")) verbose = true;
        else if (!strcmp(argv[i], "--no-claim-name")) claim_name = false;
        else if (!strcmp(argv[i], "--help") || !strcmp(argv[i], "-h")) { usage(); return 0; }
        else break;
    }
    if (!address) { fprintf(stderr, "ei-inject: no D-Bus address (use --bus)\n"); return 2; }

    int fd = get_socket_from_dbus(address, 1 /* keyboard */, claim_name);
    if (fd < 0) return 1;

    ei_ctx = ei_new_sender(NULL);
    ei_configure_name(ei_ctx, "im-harness-inject");
    if (ei_setup_backend_fd(ei_ctx, fd) != 0) {
        fprintf(stderr, "ei-inject: ei_setup_backend_fd failed\n");
        return 1;
    }

    if (!wait_ready(3000)) {
        fprintf(stderr, "ei-inject: no usable keyboard device after 3s\n");
        return 1;
    }
    vlog("ready");

    for (; i < argc; i++) {
        const char *a = argv[i];
        if (!strcmp(a, "type") && i + 1 < argc)        type_string(argv[++i], delay);
        else if (!strcmp(a, "key") && i + 1 < argc)    tap((uint32_t)atoi(argv[++i]), 20);
        else if (!strcmp(a, "down") && i + 1 < argc)   key((uint32_t)atoi(argv[++i]), true);
        else if (!strcmp(a, "up") && i + 1 < argc)     key((uint32_t)atoi(argv[++i]), false);
        else if (!strcmp(a, "sleep") && i + 1 < argc)  pump(atoi(argv[++i]));
        else if (!strcmp(a, "hold") && i + 1 < argc)   pump(atoi(argv[++i]));
        else if (!strcmp(a, "utf8") && i + 1 < argc) {
            const char *s = argv[++i];
            if (!text_ready) { fprintf(stderr, "ei-inject: no ei_text device\n"); continue; }
            ei_device_text_utf8(textdev, s);
            ei_device_frame(textdev, ei_now(ei_ctx));
            pump(20);
        } else if (!strcmp(a, "keysym") && i + 1 < argc) {
            uint32_t sym = (uint32_t)strtoul(argv[++i], NULL, 0);
            if (!text_ready) { fprintf(stderr, "ei-inject: no ei_text device\n"); continue; }
            ei_device_text_keysym(textdev, sym, true);
            ei_device_text_keysym(textdev, sym, false);
            ei_device_frame(textdev, ei_now(ei_ctx));
            pump(20);
        } else {
            fprintf(stderr, "ei-inject: unknown action '%s'\n", a);
            usage();
            return 2;
        }
    }

    if (kbd) { ei_device_stop_emulating(kbd); pump(20); }
    if (textdev) ei_device_unref(textdev);
    if (kbd) ei_device_unref(kbd);
    ei_unref(ei_ctx);
    return 0;
}

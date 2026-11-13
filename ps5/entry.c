/* Payload entry, linked in front of Rust's main with -Wl,--wrap=main. elfldr starts a payload
 * with no arguments and no terminal; this only hands over to the Rust main, which logs itself. */
extern int __real_main(int argc, char **argv);

int __wrap_main(int argc, char **argv) {
    return __real_main(argc, argv);
}

/* Minimal public ABI test doubles. No hardware operations. */
#include <stdint.h>
#include <sensors/sensors.h>
#include <sensors/error.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static void log_call(const char *message) {
    FILE *f = fopen(getenv("THINKFAN_TEST_LOG"), "a");
    if (f) { fprintf(f, "%s\n", message); fclose(f); }
}
static void block_read(const char *variable) {
    const char *path = getenv(variable);
    if (path) {
        log_call("blocked read entered");
        FILE *f = fopen(path, "r");
        if (f) { (void)fgetc(f); fclose(f); }
    }
}
unsigned nvmlInit_v2(void) { log_call("nvml init"); return 0; }
unsigned nvmlShutdown(void) { log_call("nvml shutdown"); return 0; }
unsigned nvmlDeviceGetHandleByPciBusId_v2(const char *bus, void **device) {
    if (strcmp(bus, "0000:01:00.0")) return 2;
    *device = (void *)1; return 0;
}
unsigned nvmlDeviceGetTemperature(void *device, unsigned sensor, unsigned *value) {
    if (!device || sensor) return 2;
    const char *marker = getenv("THINKFAN_TEST_NVML_FAILURE");
    FILE *failure = marker ? fopen(marker, "r") : NULL;
    if (failure) { fclose(failure); log_call("nvml read failure"); return 15; }
    log_call("nvml read success");
    *value = 60; return 0;
}
int sk_disk_open(const char *path, void **disk) {
    log_call("disk open"); *disk = strstr(path, "asleep") ? (void *)2 : (void *)1; return 0;
}
void sk_disk_free(void *disk) { (void)disk; log_call("disk free"); }
int sk_disk_check_sleep_mode(void *disk, unsigned *awake) { *awake = disk == (void *)1; return 0; }
int sk_disk_smart_read_data(void *disk) { block_read("THINKFAN_TEST_DISK_BLOCK"); log_call(disk == (void *)1 ? "awake disk read" : "sleeping disk read"); return 0; }
int sk_disk_smart_get_temperature(void *disk, uint64_t *value) { (void)disk; *value = 313150; return 0; }
static sensors_chip_name chip = { .prefix = "test", .bus = {0, 0}, .addr = 0, .path = NULL };
static sensors_feature features[] = {
    { .name = "cpu", .number = 0, .type = SENSORS_FEATURE_TEMP },
    { .name = "gpu", .number = 1, .type = SENSORS_FEATURE_TEMP }
};
static sensors_subfeature sub[] = {
    {"cpu", 0, SENSORS_SUBFEATURE_TEMP_INPUT, 0, SENSORS_MODE_R},
    {"gpu", 1, SENSORS_SUBFEATURE_TEMP_INPUT, 1, SENSORS_MODE_R}
};
void (*sensors_parse_error)(const char *, int);
void (*sensors_parse_error_wfn)(const char *, const char *, int);
void (*sensors_fatal_error)(const char *, const char *);
const char *sensors_strerror(int error) { (void)error; return "mock sensor failure"; }
int sensors_init(FILE *file) {
    (void)file; log_call("sensors init");
    const char *failure = getenv("THINKFAN_TEST_LM_FAILURE");
    if (failure && !strcmp(failure, "init")) {
        if (sensors_parse_error_wfn) sensors_parse_error_wfn("mock configuration error", "sensors3.conf", 7);
        /* Like libsensors, failed configuration parsing cleans partial state. */
        sensors_cleanup();
        return -1;
    }
    return 0;
}
void sensors_cleanup(void) { log_call("sensors cleanup"); }
const sensors_chip_name *sensors_get_detected_chips(const sensors_chip_name *match, int *cursor) { (void)match; return (*cursor)++ ? NULL : &chip; }
int sensors_snprintf_chip_name(char *out, size_t size, const sensors_chip_name *name) { (void)name; return snprintf(out, size, "test-chip"); }
const sensors_feature *sensors_get_features(const sensors_chip_name *name, int *cursor) { (void)name; if (*cursor >= 2) return NULL; return &features[(*cursor)++]; }
char *sensors_get_label(const sensors_chip_name *name, const sensors_feature *feature) {
    (void)name;
    const char *failure = getenv("THINKFAN_TEST_LM_FAILURE");
    return strdup(failure && !strcmp(failure, "duplicate") ? "CPU" : feature->number == 0 ? "CPU" : "GPU");
}
const sensors_subfeature *sensors_get_subfeature(const sensors_chip_name *name, const sensors_feature *feature, sensors_subfeature_type type) {
    (void)name;
    const char *failure = getenv("THINKFAN_TEST_LM_FAILURE");
    if (failure && !strcmp(failure, "non-temperature")) return NULL;
    return type == SENSORS_SUBFEATURE_TEMP_INPUT ? &sub[feature->number] : NULL;
}
int sensors_get_value(const sensors_chip_name *name, int number, double *value) {
    (void)name;
    const char *failure = getenv("THINKFAN_TEST_LM_FAILURE");
    if (failure && !strcmp(failure, "read")) return -1;
    if (number) block_read("THINKFAN_TEST_LM_GPU_BLOCK");
    *value = number ? 55.5 : 42.25; return 0;
}

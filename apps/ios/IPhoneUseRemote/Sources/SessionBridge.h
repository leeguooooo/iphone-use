#include <stddef.h>
char *ipu_session_open(const char *bundle, const char *password);
char *ipu_session_seal(const char *payload, const char *password);
void ipu_session_free(char *value);

/* Stand-in for AOSP's <log/log.h>, which the NDK does not ship. FDK AAC only uses it to report
 * a security event from the SBR decoder, which Sonora never runs. */
#pragma once
#define android_errorWriteLog(tag, subtag) ((void)0)

#pragma once

#ifdef __cplusplus
// Use the lite master header rather than foobar2000+atl.h: this component
// has no ATL/WTL UI and must build on machines without the ATL component.
#include <SDK/foobar2000.h>
#endif

#ifdef __OBJC__
#include <Cocoa/Cocoa.h>
#endif

/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Shared prototypes for the Accton AS4610 platform modules.
 *
 * Hemlock addition, not upstream: ONL declares these `extern` in each
 * consumer and defines them with no prototype in sight, which every
 * kernel since 5.x warns about (-Wmissing-prototypes). One header keeps
 * the CPLD accessors' signatures in one place instead of three.
 */
#ifndef ACCTON_AS4610_H
#define ACCTON_AS4610_H

#include <linux/types.h>

int as4610_54_cpld_read(unsigned short cpld_addr, u8 reg);
int as4610_54_cpld_write(unsigned short cpld_addr, u8 reg, u8 value);
int as4610_product_id(void);
int as4610_is_poe_system(void);

#endif /* ACCTON_AS4610_H */

/**
 * Copyright (c) 2018 Microsoft Open Technologies, Inc.
 *
 *    Licensed under the Apache License, Version 2.0 (the "License"); you may
 *    not use this file except in compliance with the License. You may obtain
 *    a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
 *
 *    THIS CODE IS PROVIDED ON AN *AS IS* BASIS, WITHOUT WARRANTIES OR
 *    CONDITIONS OF ANY KIND, EITHER EXPRESS OR IMPLIED, INCLUDING WITHOUT
 *    LIMITATION ANY IMPLIED WARRANTIES OR CONDITIONS OF TITLE, FITNESS
 *    FOR A PARTICULAR PURPOSE, MERCHANTABILITY OR NON-INFRINGEMENT.
 *
 *    See the Apache Version 2.0 License for specific language governing
 *    permissions and limitations under the License.
 *
 *    Microsoft would like to thank the following companies for their review and
 *    assistance with these files: Intel Corporation, Mellanox Technologies Ltd,
 *    Dell Products, L.P., Facebook, Inc., Marvell International Ltd.
 *
 * @file    saiipmcextensions.h
 *
 * @brief   This module defines IPMC extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIIPMCEXTENSIONS_H_
#define __SAIIPMCEXTENSIONS_H_

#include <saitypes.h>
#include <saiipmc.h>

/**
 * @brief IPMC event type
 */
typedef enum _sai_ipmc_event_t
{
    /** IPMC entry age */
    SAI_IPMC_EVENT_AGE,

    /** IPMC entry do not age */
    SAI_IPMC_EVENT_DONT_AGE,

    /** IPMC entry deleted */
    SAI_IPMC_EVENT_DELETED,

} sai_ipmc_event_t;

/**
 * @brief Notification data format received from SAI IPMC callback
 */
typedef struct _sai_ipmc_event_notification_data_t
{
    /** Event type */
    sai_ipmc_event_t event;

    /** IPMC entry */
    sai_ipmc_entry_t ipmc_entry;

} sai_ipmc_event_notification_data_t;

/**
 * @brief IPMC notifications
 *
 * @count data[count]
 *
 * @param[in] count Number of notifications
 * @param[in] data Pointer to IPMC event notification data array
 */
typedef void (*sai_ipmc_event_notification_fn)(
        _In_ uint32_t count,
        _In_ const sai_ipmc_event_notification_data_t *data);

#endif /* __SAIIPMCEXTENSIONS_H_ */

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
 * @file    saidebugcounterextensions.h
 *
 * @brief   This module defines debug counter extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIDEBUGCOUNTEREXTENSIONS_H_
#define __SAIDEBUGCOUNTEREXTENSIONS_H_

#include <saitypes.h>
#include <saidebugcounter.h>

/**
 * @brief SAI debug counter attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_in_drop_reason_extensions_t
{
    /**
     * @brief All Packets dropped in ingress pipeline
     *
     * This counter also supports mirroring dropped frames.
     */
    SAI_IN_DROP_REASON_ANY = SAI_IN_DROP_REASON_END,

} sai_in_drop_reason_extensions_t;

/**
 * @brief SAI debug counter attribute extensions
 *
 * @flags free
 */
typedef enum _sai_debug_counter_attr_extensions_t
{
    /**
     * @brief Mirror drop frames (mirror session id)
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_MIRROR_SESSION
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_DEBUG_COUNTER_ATTR_DROP_MIRROR_SESSION = SAI_DEBUG_COUNTER_ATTR_END,

} sai_debug_counter_attr_extensions_t;

#endif /* __SAIDEBUGCOUNTEREXTENSIONS_H_ */

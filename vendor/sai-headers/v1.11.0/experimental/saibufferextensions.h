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
 * @file    saibufferextensions.h
 *
 * @brief   This module defines buffer extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIBUFFEREXTENSIONS_H_
#define __SAIBUFFEREXTENSIONS_H_

#include <saitypes.h>

/**
 * @brief SAI buffer pool attr extensions.
 *
 * @flags free
 */
typedef enum _sai_buffer_pool_attr_extensions_t
{
    SAI_BUFFER_POOL_ATTR_EXTENSIONS_RANGE_START = SAI_BUFFER_POOL_ATTR_WRED_PROFILE_ID,

    /**
     * @brief Custom buffer pool ID
     *
     * @type sai_uint8_t
     * @flags CREATE_ONLY
     * @default 0
     */
    SAI_BUFFER_POOL_ATTR_BRCM_CUSTOM_POOL_ID,

    /**
     * @brief Custom buffer pool multicast cell queue entry size
     *
     * @type sai_uint32_t
     * @flags CREATE_ONLY
     * @default 0
     */
    SAI_BUFFER_POOL_ATTR_BRCM_CUSTOM_MCQE_SHARED_SIZE,

    SAI_BUFFER_POOL_ATTR_EXTENSIONS_RANGE_END,

} sai_buffer_pool_attr_extensions_t;

/**
 * @brief SAI buffer pool attr extensions.
 *
 * @flags free
 */
typedef enum _sai_buffer_profile_attr_extensions_t
{
    SAI_BUFFER_PROFILE_ATTR_EXTENSIONS_RANGE_START = SAI_BUFFER_PROFILE_ATTR_XON_OFFSET_TH,

    /**
     * @brief Custom buffer pool ID
     *
     * @type bool
     * @flags CREATE_ONLY
     * @default false
     */
    SAI_BUFFER_PROFILE_ATTR_BRCM_CUSTOM_USE_QGROUP_MINIMUM,

    SAI_BUFFER_PROFILE_ATTR_EXTENSIONS_RANGE_END,

} sai_buffer_profile_attr_extensions_t;

/**
 * @brief SAI buffer pool stat extensions.
 *
 * @flags free
 */
typedef enum _sai_buffer_pool_stat_extensions_t
{
    SAI_BUFFER_POOL_STAT_EXTENSIONS_RANGE_START = SAI_BUFFER_POOL_STAT_XOFF_ROOM_WATERMARK_BYTES,

    SAI_BUFFER_POOL_STAT_MULTICAST_WATERMARK_BYTES,

    SAI_BUFFER_POOL_STAT_EXTENSIONS_RANGE_END,

} sai_buffer_pool_stat_extensions_t;

#endif /* __SAIBUFFEREXTENSIONS_H_ */

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
 * @file    saiipmcgroupextensions.h
 *
 * @brief   This module defines IPMC group extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIIPMCGROUPEXTENSIONS_H_
#define __SAIIPMCGROUPEXTENSIONS_H_

#include <saitypes.h>
#include <saiipmcgroup.h>

/**
 * @brief SAI IPMC group attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_ipmc_group_member_attr_extensions_t
{
    /**
     * @brief L2MC output id
     *
     * @type sai_object_id_t
     * @flags CREATE_ONLY
     * @objects SAI_OBJECT_TYPE_L2MC_GROUP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_IPMC_GROUP_MEMBER_ATTR_EXTENSIONS_L2MC_OUTPUT_ID = SAI_IPMC_GROUP_MEMBER_ATTR_END,

    /**
     * @brief Egress Object ID
     *
     * @type sai_object_id_t
     * @flags CREATE_ONLY
     * @objects SAI_OBJECT_TYPE_NEXT_HOP,SAI_OBJECT_TYPE_NEXT_HOP_GROUP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_IPMC_GROUP_MEMBER_ATTR_EXTENSIONS_EGRESS_OBJECT

} sai_ipmc_group_member_attr_extensions_t;

#endif /* __SAIIPMCGROUPEXTENSIONS_H_ */

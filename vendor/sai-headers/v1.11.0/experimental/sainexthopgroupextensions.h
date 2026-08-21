/**
 * Copyright (c) 2021 Microsoft Open Technologies, Inc.
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
 * @file    sainexthopgroupextensions.h
 *
 * @brief   This module defines nexthop group extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAINEXTHOPGROUPEXTENSIONS_H_
#define __SAINEXTHOPGROUPEXTENSIONS_H_

#include <saitypes.h>
#include <sainexthopgroup.h>

/**
 * @brief SAI nexthop group attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_next_hop_group_attr_extensions_t
{
    /**
     * @brief Hierarchical next hop group level.
     * true: Nexthop group consists of tunnel and IP nexthop
     * false: Nexthop group consists of IP nexthop only
     *
     * @type bool
     * @flags CREATE_ONLY
     * @default true
     * @validonly SAI_NEXT_HOP_GROUP_ATTR_TYPE == SAI_NEXT_HOP_GROUP_TYPE_DYNAMIC_UNORDERED_ECMP or SAI_NEXT_HOP_GROUP_ATTR_TYPE == SAI_NEXT_HOP_GROUP_TYPE_DYNAMIC_ORDERED_ECMP
     * @isresourcetype true
     */
    SAI_NEXT_HOP_GROUP_ATTR_HIERARCHICAL_NEXTHOP = SAI_NEXT_HOP_GROUP_ATTR_END

} sai_next_hop_group_attr_extensions_t;

#endif /* __SAINEXTHOPGROUPEXTENSIONS_H_ */

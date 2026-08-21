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
 * @file    sainexthopextensions.h
 *
 * @brief   This module defines nexthop extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAINEXTHOPEXTENSIONS_H_
#define __SAINEXTHOPEXTENSIONS_H_

#include <saitypes.h>
#include <sainexthop.h>

/**
 * @brief SAI nexthop attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_next_hop_attr_extensions_t
{
    /**
     * @brief Next hop entry router interface id
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_ROUTER_INTERFACE
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     * @validonly SAI_NEXT_HOP_ATTR_TYPE == SAI_NEXT_HOP_TYPE_TUNNEL_ENCAP
     */
    SAI_NEXT_HOP_ATTR_TUNNEL_ROUTER_INTERFACE_ID = SAI_NEXT_HOP_ATTR_END

} sai_next_hop_attr_extensions_t;

#endif /* __SAINEXTHOPEXTENSIONS_H_ */

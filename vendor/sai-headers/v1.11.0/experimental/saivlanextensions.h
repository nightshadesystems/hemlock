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
 * @file    saivlanextensions.h
 *
 * @brief   This module defines vlan extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIVLANEXTENSIONS_H_
#define __SAIVLANEXTENSIONS_H_

#include <saitypes.h>
#include <saivlan.h>

/**
 * @brief SAI vlan attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_vlan_attr_extensions_t
{
    /**
     * @brief To enable neighbour suppress on a VLAN
     *
     * This feature stops the flooding of ARP and ND messages over Tunnel.
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_VLAN_ATTR_NEIGH_SUPPRESS_ENABLE = SAI_VLAN_ATTR_END

} sai_vlan_attr_extensions_t;

#endif /* __SAIVLANEXTENSIONS_H_ */

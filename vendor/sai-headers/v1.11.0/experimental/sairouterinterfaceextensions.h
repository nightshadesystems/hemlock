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
 * @file    sairouterinterfaceextensions.h
 *
 * @brief   This module defines router interface extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIROUTERINTERFACEEXTENSIONS_H_
#define __SAIROUTERINTERFACEEXTENSIONS_H_

#include <saitypes.h>
#include <sairouterinterface.h>

/**
 * @brief SAI router interface attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_router_interface_attr_extensions_t
{
    /**
     * @brief SAG support
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_ROUTER_INTERFACE_ATTR_ANYCAST_MAC_SUPPORT = SAI_ROUTER_INTERFACE_ATTR_END,

    /**
     * @brief MAC Address
     *
     * valid when SAI_ROUTER_INTERFACE_ATTR_TYPE != SAI_ROUTER_INTERFACE_TYPE_LOOPBACK
     *
     * @type sai_mac_t
     * @flags CREATE_AND_SET
     * @default attrvalue SAI_VIRTUAL_ROUTER_ATTR_SRC_MAC_ADDRESS
     */
    SAI_ROUTER_INTERFACE_ATTR_SRC_MAC_ADDRESS_RIF_UPDATE,

    /**
     * @brief Enable DSCP -> TC MAP
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_QOS_MAP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_ROUTER_INTERFACE_ATTR_QOS_DSCP_TO_TC_MAP,

    /**
     * @brief Enable DSCP -> COLOR MAP
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_QOS_MAP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_ROUTER_INTERFACE_ATTR_QOS_DSCP_TO_COLOR_MAP,

    /**
     * @brief Enable TC AND COLOR -> DSCP MAP
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_QOS_MAP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_ROUTER_INTERFACE_ATTR_QOS_TC_AND_COLOR_TO_DSCP_MAP,

    /**
     * @brief MAC Address
     *
     * valid when SAI_ROUTER_INTERFACE_ATTR_TYPE != SAI_ROUTER_INTERFACE_TYPE_LOOPBACK
     *
     * @type sai_mac_t
     * @flags CREATE_AND_SET
     * @default attrvalue SAI_VIRTUAL_ROUTER_ATTR_SRC_MAC_ADDRESS
     */
    SAI_ROUTER_INTERFACE_ATTR_PEER_SRC_MAC_ADDRESS

} sai_router_interface_attr_extensions_t;

#endif /* __SAIROUTERINTERFACEEXTENSIONS_H_ */

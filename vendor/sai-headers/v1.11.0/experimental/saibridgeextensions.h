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
 * @file    saibridgeextensions.h
 *
 * @brief   This module defines bridge extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIBRIDGEEXTENSIONS_H_
#define __SAIBRIDGEEXTENSIONS_H_

#include <saitypes.h>
#include <saibridge.h>

/**
 * @brief SAI bridge port attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_bridge_port_attr_extensions_t
{
    /**
     * @brief Inter-Switch Link for Multi-Chassis LAG
     *
     * Enable inter-switch link for multi-chassis LAG. Default is disable
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_BRIDGE_PORT_ATTR_EXPERIMENTAL_MLAG_ISL = SAI_BRIDGE_PORT_ATTR_END

} sai_bridge_port_attr_extensions_t;

#endif /* __SAIBRIDGEEXTENSIONS_H_ */

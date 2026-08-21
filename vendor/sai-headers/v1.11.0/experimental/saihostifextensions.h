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
 * @file    saihostifextensions.h
 *
 * @brief   This module defines host interface extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIHOSTIFEXTENSIONS_H_
#define __SAIHOSTIFEXTENSIONS_H_

#include <saitypes.h>
#include <saihostif.h>

/**
 * @brief SAI host interface trap type extensions.
 *
 * @flags free
 */
typedef enum _sai_hostif_trap_type_extensions_t
{
    /**
     * @brief ARP Suppression Packet Trap
     * (packet action will be trap)
     */
    SAI_HOSTIF_TRAP_TYPE_ARP_SUPPRESS = SAI_HOSTIF_TRAP_TYPE_END,

    /**
     * @brief ND Suppression Packet Trap
     * (packet action will be trap)
     */
    SAI_HOSTIF_TRAP_TYPE_ND_SUPPRESS,

    /**
     * @brief ICMP traffic (IP protocol == 1) to local router IP address
     * (default packet action is drop)
     */
    SAI_HOSTIF_TRAP_TYPE_ICMP,

    /**
     * @brief ICMPV6 traffic (IP next header == 58) to local router IP address
     * (default packet action is drop)
     */
    SAI_HOSTIF_TRAP_TYPE_ICMPV6,

    /**
     * @brief Inter chassis control protocol traffic
     * (TCP dst port == 8888 or TCP src port == 8888) to local router IP address
     * (default packet action is drop)
     */
    SAI_HOSTIF_TRAP_TYPE_ICCP,

    /**
     * @brief Copy known multicast data packet to CPU
     * Known multicast data packets hitting (S,G) or (*,g) entries for which
     * SAI_PACKET_ACTION_COPY is set will be copied to CPU.
     * (IP multicast IP = 224.0.0.0/4)
     * (Only packet action supported is copy)
     */
    SAI_HOSTIF_TRAP_TYPE_KNOWN_L3_MULTICAST

} sai_hostif_trap_type_extensions_t;

#endif /* __SAIHOSTIFEXTENSIONS_H_ */

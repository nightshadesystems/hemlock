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
 * @file    saiportextensions.h
 *
 * @brief   This module defines port extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIPORTEXTENSIONS_H_
#define __SAIPORTEXTENSIONS_H_

#include <saitypes.h>
#include <saiport.h>

/**
 * @brief SAI physical level status bits
 *
 * @flags strict
 */
typedef enum _sai_pmd_status_t
{
    SAI_PMD_STATUS_SIGNAL_DETECT = 1 << 0,
    SAI_PMD_STATUS_CDR_LOCK = 1 << 1
} sai_pmd_status_t;

/**
 * @brief SAI PCS status bits
 *
 * @flags strict
 */
typedef enum _sai_pcs_status_t
{
    SAI_PCS_STATUS_SYNC = 1 << 0,
    SAI_PCS_STATUS_LINK = 1 << 1,
    SAI_PCS_STATUS_LOCAL_FAULT = 1 << 2,
    SAI_PCS_STATUS_REMOTE_FAULT = 1 << 3,
    SAI_PCS_STATUS_HI_BER = 1 << 4,
    SAI_PCS_STATUS_DESKEW = 1 << 5,
    SAI_PCS_STATUS_AM_LOCK = 1 << 6,
    SAI_PCS_STATUS_AMPS_LOCK = 1 << 7,
    SAI_PCS_STATUS_BLOCK_LOCK = 1 << 8
} sai_pcs_status_t;

/**
 * @brief SAI port attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_port_attr_extensions_t
{
    /**
     * @brief Enable/Disable Port UNRELIABLE loss of signal
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_PORT_ATTR_UNRELIABLE_LOS_ENABLE = SAI_PORT_ATTR_END,

    /**
     * @brief Port state handling for fast convergence
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_PORT_ATTR_PORT_STATE_FAST_CONVERGENCE,

    /**
     * @brief Enable/Disable port rx lane squelch
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_PORT_ATTR_RX_LANE_SQUELCH_ENABLE,

    /**
     * @brief Perform cable testing and diagnostics
     *
     * @type sai_uint32_t
     * @flags READ_ONLY
     */
    SAI_PORT_ATTR_PORT_CABLE_DIAGNOSTICS,

    /**
     * @brief Port-Locator LED mode
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_PORT_ATTR_LED_LOCATOR_MODE,

    /**
     * @brief Vendor specific port ACL attribute extension
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_ACL_TABLE, SAI_OBJECT_TYPE_ACL_TABLE_GROUP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_PORT_ATTR_LOOKUP_ACL,

    /**
     * @brief Enable/Disable Port diagnostics mode. Port is taken out of link scan bit map.
     *
     * This feature can be used for any port diagnostic feature. This is used now to fetch PCS status.
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_PORT_ATTR_DIAGNOSTICS_MODE_ENABLE,

    /**
     * @brief Physical level status bitmap
     *
     * @type sai_uint32_t
     * @flags READ_ONLY
     */
    SAI_PORT_ATTR_PMD_STATUS_BITMAP,

    /**
     * @brief PCS status bitmap
     *
     * @type sai_uint32_t
     * @flags READ_ONLY
     */
    SAI_PORT_ATTR_PCS_STATUS_BITMAP,

    /**
     * @brief Gather port debug information.
     *
     * Gather vendor-specific debug information about the port. The returned
     * data should be in the form of a null-terminated string.
     *
     * @type sai_s8_list_t
     * @flags READ_ONLY
     */
    SAI_PORT_ATTR_DEBUG_DATA,

    /**
     * @brief Serdes/PMD Lane list
     *
     * @type sai_u32_list_t
     * @flags READ_ONLY
     */
    SAI_PORT_ATTR_SERDES_LANE_LIST

} sai_port_attr_extensions_t;

/**
 * @brief SAI port breakout mode type extensions.
 *
 * @flags free
 */
typedef enum _sai_port_breakout_mode_type_extensions_t
{
    /** 8 lanes breakout Mode */
    SAI_PORT_BREAKOUT_MODE_TYPE_8_LANE = SAI_PORT_BREAKOUT_MODE_TYPE_MAX,

    /** Breakout mode max count */
    SAI_PORT_BREAKOUT_MODE_TYPE_MAX_EXTN
} sai_port_breakout_mode_type_extensions_t;

/**
 * @brief List of Port Serdes attribute extensions
 *
 * @flags free
 */
typedef enum _sai_port_serdes_attr_extensions_t
{
    /**
     * @brief Port serdes control TX TAP MODE
     *
     * List of port serdes TX tap mode values.
     * The values are of type sai_u32_list_t where the count is number of lanes
     * in a port and the list specifies list of values to be applied to each
     * lane.
     *
     * @type sai_u32_list_t
     * @flags CREATE_ONLY
     * @default internal
     */
    SAI_PORT_SERDES_ATTR_TX_TAP_MODE = SAI_PORT_SERDES_ATTR_END,

    /**
     * @brief Port serdes control TX SIGNAL MODE
     *
     * List of port serdes TX signal mode values.
     * The values are of type sai_u32_list_t where the count is number of lanes
     * in a port and the list specifies list of values to be applied to each
     * lane.
     *
     * @type sai_u32_list_t
     * @flags CREATE_ONLY
     * @default internal
     */
    SAI_PORT_SERDES_ATTR_TX_SIGNAL_MODE,

    /**
     * @brief Port serdes control TX AMPLITUDE
     *
     * List of port serdes TX amplitude values.
     * The values are of type sai_u32_list_t where the count is number of lanes
     * in a port and the list specifies list of values to be applied to each
     * lane.
     *
     * @type sai_u32_list_t
     * @flags CREATE_ONLY
     * @default internal
     */
    SAI_PORT_SERDES_ATTR_TX_AMPLITUDE

} sai_port_serdes_attr_extensions_t;

/**
 * @brief SAI port stat extensions.
 *
 * @flags free
 */
typedef enum _sai_port_stat_extensions_t
{
    SAI_PORT_STAT_EXTENSIONS_RANGE_START = SAI_PORT_STAT_OUT_CONFIGURED_DROP_REASONS_7_DROPPED_PKTS + 0x1,

    /** SAI port stat if in Bit Error Rate */
    SAI_PORT_STAT_IF_IN_BER_COUNT = SAI_PORT_STAT_EXTENSIONS_RANGE_START,

    /** SAI port stat if in Error Block Count */
    SAI_PORT_STAT_IF_IN_ERROR_BLOCK_COUNT,

    /** SAI port stat if in Bit Interleaved Parity */
    SAI_PORT_STAT_IF_IN_BIP_ERROR_COUNT,

    SAI_PORT_STAT_EXTENSIONS_RANGE_END

} sai_port_stat_extensions_t;

/**
 * @brief Attribute data for port event notification
 */
typedef enum _sai_port_event_t
{
    /** Unknown */
    SAI_PORT_EVENT_UNKNOWN,

    /** Remote-fault */
    SAI_PORT_EVENT_REMOTE_FAULT,

    /** Local-fault */
    SAI_PORT_EVENT_LOCAL_FAULT,

    /** Pre-emphasis failed */
    SAI_PORT_EVENT_PREEMPHASIS_FAILED,

    /** FEC set failed */
    SAI_PORT_EVENT_FEC_FAILED,

    /** Speed set failed */
    SAI_PORT_EVENT_SPEED_FAILED,

    /** Interface type set failed */
    SAI_PORT_EVENT_IF_TYPE_FAILED,

    /** Media type set failed */
    SAI_PORT_EVENT_MEDIA_TYPE_FAILED,

    /** Link Training set failed */
    SAI_PORT_EVENT_LINK_TRAINING_FAILED,

    /** Port PCS Errors */
    SAI_PORT_EVENT_PCS_ERRORS

} sai_port_event_t;

/**
 * @brief Defines different events for the port
 */
typedef struct _sai_port_event_notification_t
{
    /**
     * @brief Port id.
     *
     * @objects SAI_OBJECT_TYPE_PORT
     */
    sai_object_id_t port_id;

    /** Port event */
    sai_port_event_t event;

} sai_port_event_notification_t;

/**
 * @brief Port event notification
 *
 * @count data[count]
 *
 * @param[in] count Number of notifications
 * @param[in] data Array of port events
 */
typedef void (*sai_port_events_notification_fn)(
        _In_ uint32_t count,
        _In_ const sai_port_event_notification_t *data);

/**
 * @brief SAI port pool extensions.
 *
 * @flags free
 */
typedef enum _sai_port_pool_attr_extensions_t
{
    SAI_PORT_POOL_ATTR_EXTENSIONS_RANGE_START = SAI_PORT_POOL_ATTR_QOS_WRED_PROFILE_ID,

    /**
     * @brief Port pool bind point for TAM object
     *
     * @type sai_object_list_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_TAM
     * @default empty
     */
    SAI_PORT_POOL_ATTR_TAM_OBJECT,

    SAI_PORT_POOL_ATTR_EXTENSIONS_RANGE_END,

} sai_port_pool_attr_extensions_t;

/**
 * @brief SAI port pool stat extensions.
 *
 * @flags free
 */
typedef enum _sai_port_pool_stat_extensions_t
{
    SAI_PORT_POOL_STAT_EXTENSIONS_RANGE_START = SAI_PORT_POOL_STAT_DROPPED_PKTS,

    SAI_PORT_POOL_STAT_UNICAST_WATERMARK_BYTES,

    SAI_PORT_POOL_STAT_EXTENSIONS_RANGE_END,

} sai_port_pool_stat_extensions_t;

#endif /* __SAIPORTEXTENSIONS_H_ */


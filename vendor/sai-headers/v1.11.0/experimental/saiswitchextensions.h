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
 * @file    saiswitchextensions.h
 *
 * @brief   This module defines switch extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAISWITCHEXTENSIONS_H_
#define __SAISWITCHEXTENSIONS_H_

#include <saitypes.h>

/**
 * @def SAI_KEY_LED_PORT_LOCATOR_FW_FILE
 */
#define SAI_KEY_LED_PORT_LOCATOR_FW_FILE            "SAI_LED_PORT_LOCATOR_FW_FILE"

/**
 * @def SAI_KEY_LED_PORT_LOCATOR_CONFIG_FILE
 */
#define SAI_KEY_LED_PORT_LOCATOR_CONFIG_FILE        "SAI_LED_PORT_LOCATOR_CONFIG_FILE"

/**
 * @brief Switch event callback
 *
 * @count buffer[buffer_size]
 *
 * @param[in] buffer_size Actual buffer size in bytes
 * @param[in] buffer Data buffer
 * @param[in] u32 Switch event
 */
typedef void (*sai_switch_event_notification_fn)(
        _In_ sai_size_t buffer_size,
        _In_ const void *buffer,
        _In_ uint32_t u32);

/**
 * @brief SAI switch attribute extensions.
 *
 * @flags free
 */
typedef enum _sai_switch_attr_extensions_t
{
    SAI_SWITCH_ATTR_EXTENSIONS_RANGE_START = SAI_SWITCH_ATTR_END,

    /**
     * @brief Set LED
     *
     * @type sai_u32_list_t
     * @flags CREATE_AND_SET
     * @default empty
     */
    SAI_SWITCH_ATTR_LED = SAI_SWITCH_ATTR_EXTENSIONS_RANGE_START,

    /**
     * @brief Reset LED Microprocessor
     *
     * @type sai_u32_list_t
     * @flags CREATE_AND_SET
     * @default empty
     */
    SAI_SWITCH_ATTR_LED_PROCESSOR_RESET,

    /**
     * @brief Port-Locator LED mode
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_SWITCH_ATTR_LED_LOCATOR_MODE,

    /**
     * @brief Set Switch Capability Extension
     *
     * @type sai_u32_list_t
     * @flags CREATE_ONLY
     * @default empty
     */
    SAI_SWITCH_ATTR_CAPABILITY_EXTENSION,

    /**
     * @brief Get Switch Default Egress Buffer Size Extension
     *
     * @type sai_uint32_t
     * @flags READ_ONLY
     */
    SAI_SWITCH_ATTR_DEFAULT_EGRESS_BUFFER_POOL_SHARED_SIZE,

    /**
     * @brief Enable/Disable ECMP Symmetric Hash for IPv4 flows
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_SWITCH_ATTR_ECMP_SYMMETRIC_HASH_IPV4,

    /**
     * @brief Enable/Disable ECMP Symmetric Hash for IPv6 flows
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_SWITCH_ATTR_ECMP_SYMMETRIC_HASH_IPV6,

    /**
     * @brief Event notification callback
     * function passed to the adapter.
     *
     * Use sai_switch_event_notification_fn as notification function.
     *
     * @type sai_pointer_t sai_switch_event_notification_fn
     * @flags CREATE_AND_SET
     * @default NULL
     */
    SAI_SWITCH_ATTR_SWITCH_EVENT_NOTIFY,

    /**
     * @brief Event Identifier
     *
     * @type sai_uint32_t
     * @flags READ_ONLY
     */
    SAI_SWITCH_ATTR_SWITCH_EVENT_ID,

    /**
     * @brief Switch events
     *
     * @type sai_u32_list_t
     * @flags CREATE_AND_SET
     * @default empty
     */
    SAI_SWITCH_ATTR_SWITCH_EVENT_TYPE,

    /**
     * @brief Enable/Disable ECC event throttling
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_SWITCH_ATTR_ECC_EVENT_THROTTLE_ENABLE,

    /**
     * @brief Enable/disable CL22 MDIO
     *
     * @type bool
     * @flags CREATE_AND_SET
     * @default false
     */
    SAI_SWITCH_ATTR_MDIO_CL22,

    /**
     * @brief Switch or Global bind point for Pre-ingress Exact Match ACL object
     *
     * @type sai_object_id_t
     * @flags CREATE_AND_SET
     * @objects SAI_OBJECT_TYPE_ACL_TABLE,SAI_OBJECT_TYPE_ACL_TABLE_GROUP
     * @allownull true
     * @default SAI_NULL_OBJECT_ID
     */
    SAI_SWITCH_ATTR_PRE_INGRESS_EM_ACL,

    /**
     * @brief Received packet event notification callback function passed to the adapter.
     *
     * Use sai_port_events_notification_fn as notification function.
     *
     * @type sai_pointer_t sai_port_events_notification_fn
     * @flags CREATE_AND_SET
     * @default NULL
     */
    SAI_SWITCH_ATTR_PORT_EVENTS_NOTIFY,

    /**
     * @brief Get Switch Resource scale type
     *
     * @type sai_switch_resource_type_t
     * @flags READ_ONLY
     */
    SAI_SWITCH_ATTR_RESOURCE_TYPE,

    /**
     * @brief Custom DLL path for ISSU operation
     *
     * @type sai_s8_list_t
     * @flags CREATE_ONLY
     * @default empty
     */
    SAI_SWITCH_ATTR_ISSU_CUSTOM_DLL_PATH,

    /**
     * @brief PTP capability supported by the NPU
     *
     * @type sai_switch_ptp_capability_t
     * @flags READ_ONLY
     */
    SAI_SWITCH_ATTR_PTP_CAPABILITY,

    /**
     * @brief Dynamic IPMC entry aging time in seconds
     * Zero means aging is disabled.
     *
     * @type sai_uint32_t
     * @flags CREATE_AND_SET
     * @default 0
     */
    SAI_SWITCH_ATTR_IPMC_AGING_TIME,

    /**
     * @brief IPMC event notification callback function passed to the adapter.
     *
     * Use sai_ipmc_event_notification_fn as notification function.
     *
     * @type sai_pointer_t sai_ipmc_event_notification_fn
     * @flags CREATE_AND_SET
     * @default NULL
     */
    SAI_SWITCH_ATTR_IPMC_EVENT_NOTIFY,

    /**
     * @brief Get the CPU Recycle Port
     *
     * @type sai_object_id_t
     * @flags READ_ONLY
     * @objects SAI_OBJECT_TYPE_PORT
     * @default internal
     */
    SAI_SWITCH_ATTR_CPU_RCY_PORT,

    SAI_SWITCH_ATTR_EXTENSIONS_RANGE_END
} sai_switch_attr_extensions_t;

/**
 * @brief Switch Event Type.
 */
typedef enum _sai_switch_event_type_t
{
    /** Stable Full */
    SAI_SWITCH_EVENT_TYPE_STABLE_FULL,

    /** Stable Error */
    SAI_SWITCH_EVENT_TYPE_STABLE_ERROR,

    /** Uncontrolled Shutdown */
    SAI_SWITCH_EVENT_TYPE_UNCONTROLLED_SHUTDOWN,

    /** Downgrade during Warm Boot */
    SAI_SWITCH_EVENT_TYPE_WARM_BOOT_DOWNGRADE,

    /** Parity Error */
    SAI_SWITCH_EVENT_TYPE_PARITY_ERROR,

    /** Last Switch event type */
    SAI_SWITCH_EVENT_TYPE_MAX
} sai_switch_event_type_t;

/**
 * @brief SAI switch soft error recovery error type.
 */
typedef enum _sai_switch_error_type_t
{
    /**
     * @brief Unknown error type
     */
    SAI_SWITCH_ERROR_TYPE_UNKNOWN = 0,

    /**
     * @brief Parity error
     */
    SAI_SWITCH_ERROR_TYPE_PARITY = 1,

    /**
     * @brief ECC single bit error
     */
    SAI_SWITCH_ERROR_TYPE_ECC_SINGLE_BIT = 2,

    /**
     * @brief ECC double bit error
     */
    SAI_SWITCH_ERROR_TYPE_ECC_DOUBLE_BIT = 3,

    /**
     * @brief All error types
     */
    SAI_SWITCH_ERROR_TYPE_ALL = 4
} sai_switch_error_type_t;

/**
 * @brief SAI switch soft error recovery error correction type.
 */
typedef enum _sai_switch_correction_type_t
{
    /**
     * @brief S/W takes no action when error
     * happens (Like some working memories)
     */
    SAI_SWITCH_CORRECTION_TYPE_NO_ACTION = 0,

    /**
     * @brief S/W tries to correct error but fails
     */
    SAI_SWITCH_CORRECTION_TYPE_FAIL_TO_CORRECT = 1,

    /**
     * @brief S/W writes NULL entry to clear the error
     */
    SAI_SWITCH_CORRECTION_TYPE_ENTRY_CLEAR = 2,

    /**
     * @brief Restore entry from a valid S/W cache
     */
    SAI_SWITCH_CORRECTION_TYPE_CACHE_RESTORE = 3,

    /**
     * @brief Restore entry from another pipe
     */
    SAI_SWITCH_CORRECTION_TYPE_HW_CACHE_RESTORE = 4,

    /**
     * @brief Memory needs special correction handling
     */
    SAI_SWITCH_CORRECTION_TYPE_SPECIAL = 5,

    /**
     * @brief All correction types
     */
    SAI_SWITCH_CORRECTION_TYPE_ALL = 6
} sai_switch_correction_type_t;

/* soft error recovery log info flags */
#define SAI_SWITCH_SER_LOG_MEM          0x00000001 /* Error happens on memory */
#define SAI_SWITCH_SER_LOG_REG          0x00000002 /* Error happens on register */
#define SAI_SWITCH_SER_LOG_MULTI        0x00000004 /* Parity errors detected more than once */
#define SAI_SWITCH_SER_LOG_CORRECTED    0x00000008 /* Error be corrected by S/W */
#define SAI_SWITCH_SER_LOG_ENTRY        0x00000010 /* Corrupt entry data is valid */
#define SAI_SWITCH_SER_LOG_CACHE        0x00000020 /* Cache data is valid */

/**
 * @brief SAI switch soft error recovery log info.
 */
typedef struct _sai_switch_ser_log_info_t
{
    /**
     * @brief Error detected time
     */
    uint32_t time;

    /**
     * @brief Soft error recovery log info flags
     */
    uint32_t flags;

    /**
     * @brief Error type
     */
    sai_switch_error_type_t error_type;

    /**
     * @brief Correction type
     */
    sai_switch_correction_type_t correction_type;
} sai_switch_ser_log_info_t;

/**
 * @brief Attribute extension
 *
 * @flags free
 */
typedef enum _sai_switch_oper_status_extensions_t
{
    SAI_SWITCH_OPER_STATUS_EXTENSIONS_RANGE_START = SAI_SWITCH_OPER_STATUS_FAILED,

    SAI_SWITCH_OPER_STATUS_EXTENSIONS_PCI_TIMEOUT_ERROR,

    SAI_SWITCH_OPER_STATUS_EXTENSIONS_HIGH_TEMP,

    SAI_SWITCH_OPER_STATUS_EXTENSIONS_RANGE_END,

} sai_switch_oper_status_extensions_t;

/**
 * @brief SAI switch stat extensions.
 *
 * @flags free
 */
typedef enum _sai_switch_stat_extensions_t
{
    SAI_SWITCH_STAT_EXTENSIONS_RANGE_START = SAI_SWITCH_STAT_FABRIC_DROP_REASON_RANGE_END,

    SAI_SWITCH_STAT_DEVICE_WATERMARK_BYTES,

    SAI_SWITCH_STAT_EXTENSIONS_RANGE_END,

} sai_switch_stat_extensions_t;

/**
 * @brief Attribute data for #SAI_SWITCH_ATTR_PTP_CAPABILITY
 */
typedef enum _sai_switch_ptp_capability_t
{
    /** NPU doesn't support PTP */
    SAI_SWITCH_PTP_CAPABILITY_NONE = 0,

    /** 1-step only */
    SAI_SWITCH_PTP_CAPABILITY_1STEP = 1,

    /** 2-step only */
    SAI_SWITCH_PTP_CAPABILITY_2STEP = 2,

    /** Both 1-step and 2-step supported */
    SAI_SWITCH_PTP_CAPABILITY_BOTH_1STEP_AND_2STEP = 3

} sai_switch_ptp_capability_t;

/**
 * @brief SAI switch resource allocation of hardware L2 and L3 memory entries.
 * Currently there are three resource allocation modes
 * route-scale routes max
 * route-scale hosts layer2-layer3
 * drop-monitor flows min/max/none
 */
typedef enum _sai_switch_resource_type_t
{
    SAI_SWITCH_RESOURCE_TYPE_DEFAULT,

    SAI_SWITCH_RESOURCE_TYPE_HOST_SCALE,

    SAI_SWITCH_RESOURCE_TYPE_UAT_MODE

} sai_switch_resource_type_t;

/**
 * @def SAI_KEY_CUSTOM_KERNEL_BDE_NAME
 * Board specific device enumerator name for kernel module, can be used to override the SDK default
 */
#define SAI_KEY_CUSTOM_KERNEL_BDE_NAME            "SAI_CUSTOM_KERNEL_BDE_NAME"

/**
 * @def SAI_KEY_CUSTOM_USER_BDE_NAME
 * Board specific device enumerator name for user module, can be used to override the SDK default
 */
#define SAI_KEY_CUSTOM_USER_BDE_NAME              "SAI_CUSTOM_USER_BDE_NAME"

/**
 * @def SAI_KEY_CUSTOM_NON_ECMP_MAX_SIZE
 * Partition size for non ECMP entries, can be used to dedicate a range of forwarding entry index
 * for the non ECMP entries, and the remaining for ECMP groups.
 */
#define SAI_KEY_CUSTOM_NON_ECMP_MAX_SIZE          "SAI_SWITCH_NON_ECMP_MAX_SIZE"

#endif /* __SAISWITCHEXTENSIONS_H_ */

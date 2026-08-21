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
 * @file    saipolicerextensions.h
 *
 * @brief   This module defines Policer extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIPOLICEREXTENSIONS_H_
#define __SAIPOLICEREXTENSIONS_H_

#include <saipolicer.h>

/**
 * @brief SAI policer attributes extensions.
 *
 * @flags free
 */
typedef enum _sai_policer_attr_extensions_t
{
    SAI_POLICER_ATTR_EXTENSIONS_START = SAI_POLICER_ATTR_END,

    /**
     * @brief Operational committed burst size bytes/packets based on
     * #SAI_POLICER_ATTR_METER_TYPE
     *
     * @type sai_uint64_t
     * @flags READ_ONLY
     */
    SAI_POLICER_ATTR_OPERATIONAL_CBS = SAI_POLICER_ATTR_EXTENSIONS_START,

    /**
     * @brief Operational committed information rate BPS/PPS based on
     * #SAI_POLICER_ATTR_METER_TYPE
     *
     * @type sai_uint64_t
     * @flags READ_ONLY
     */
    SAI_POLICER_ATTR_OPERATIONAL_CIR,

    /**
     * @brief Operational peak burst size bytes/packets based on
     * #SAI_POLICER_ATTR_METER_TYPE
     *
     * @type sai_uint64_t
     * @flags READ_ONLY
     */
    SAI_POLICER_ATTR_OPERATIONAL_PBS,

    /**
     * @brief Operational peak information rate BPS/PPS based on
     * #SAI_POLICER_ATTR_METER_TYPE
     *
     * @type sai_uint64_t
     * @flags READ_ONLY
     */
    SAI_POLICER_ATTR_OPERATIONAL_PIR,

    SAI_POLICER_ATTR_EXTENSIONS_END
} sai_policer_attr_extensions_t;

#endif /* __SAIPOLICEREXTENSIONS_H_ */

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
 * @file    saifdbextensions.h
 *
 * @brief   This module defines FDB extensions of the Switch Abstraction Interface (SAI)
 */

#ifndef __SAIFDBEXTENSIONS_H_
#define __SAIFDBEXTENSIONS_H_

#include <saifdb.h>

/**
 * @brief SAI FDB type extensions.
 *
 * @flags free
 */
typedef enum _sai_fdb_entry_type_extensions_t
{
    SAI_FDB_ENTRY_TYPE_EXTENSIONS_START = SAI_FDB_ENTRY_TYPE_STATIC,

    /** Static FDB Entry with Move capability */
    SAI_FDB_ENTRY_TYPE_STATIC_MACMOVE,

    SAI_FDB_ENTRY_TYPE_EXTENSIONS_END
} sai_fdb_entry_type_extensions_t;

#endif /* __SAIFDBEXTENSIONS_H_ */
